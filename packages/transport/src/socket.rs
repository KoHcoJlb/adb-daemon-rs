use crate::connection::ConnectionTrait;
use crate::error::ErrorKind;
use crate::message::{AdbCommand, AdbMessage};
use crate::transport::{ConnectionExt, TransportBackend, MAX_PAYLOAD};
use crate::{Error, Result};
use bytes::{Buf, BufMut};
use diatomic_waker::{WakeSink, WakeSource};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::spawn;
use tokio::time::{interval, Interval};
use tracing::span::EnteredSpan;
use tracing::{debug, info, info_span, warn, Span};

fn io_error(cause: impl Into<Error>) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, cause.into())
}

pub(crate) struct State {
    pub remote_id: u32,
    pub closed: bool,

    read_buffer: VecDeque<Cursor<Vec<u8>>>,

    initial_acknowledgment: isize,
    acknowledged: isize,
    write_throttle: Interval,
}

pub(crate) struct SocketBackend {
    transport: Arc<TransportBackend>,
    local_id: u32,
    span: Span,

    pub read_waker: WakeSource,
    pub write_waker: WakeSource,

    pub state: Mutex<State>,
}

impl SocketBackend {
    #[inline]
    fn check_transport_error(&self) -> io::Result<()> {
        if let Some(err) = self.transport.error.get() {
            debug!(?err, "transport error");
            Err(io_error(ErrorKind::TransportError(err.clone())))
        } else {
            Ok(())
        }
    }

    #[inline]
    fn enter_span(&self) -> EnteredSpan {
        self.span.clone().entered()
    }

    fn shutdown(&self) -> Option<AdbMessage> {
        if self.check_transport_error().is_ok() {
            let mut state = self.state.lock();
            if !state.closed {
                state.closed = true;
                return Some(AdbMessage::new(
                    AdbCommand::CLSE,
                    self.local_id,
                    state.remote_id,
                    Vec::new(),
                ));
            }
        }

        None
    }

    pub fn handle_message(&self, msg: AdbMessage) -> Result<bool> {
        let _span = self.enter_span();

        let mut state = self.state.lock();

        match msg.header.command {
            AdbCommand::OKAY => {
                let avail = u32::from_le_bytes(
                    msg.payload.try_into().map_err(|_| (ErrorKind::Other, "no avail in OKAY"))?,
                );

                if state.remote_id == 0 {
                    state.initial_acknowledgment = avail as isize;
                    state.remote_id = msg.header.arg0;

                    self.span.record("remote_id", state.remote_id);
                }

                debug!(acknowledged = state.acknowledged, avail);
                state.acknowledged += avail as isize;

                self.write_waker.notify();
            }
            AdbCommand::WRTE => {
                state.read_buffer.push_back(Cursor::new(msg.payload));
                self.read_waker.notify();
            }
            AdbCommand::CLSE => {
                state.closed = true;
                self.write_waker.notify();
                self.read_waker.notify();
            }
            _ => unreachable!("other messages should not end up here"),
        }

        Ok(state.closed)
    }
}

impl Drop for SocketBackend {
    fn drop(&mut self) {
        let _span = self.enter_span();

        if let Some(msg) = self.shutdown() {
            warn!("dropping non closed socket");
            let transport = self.transport.clone();
            spawn(async move {
                let _ = transport.write_message_async(msg).await;
            });
        } else {
            debug!("dropping socket backend");
        }
    }
}

pub struct Socket {
    pub(crate) inner: Arc<SocketBackend>,
    pub(crate) read_waker: WakeSink,
    pub(crate) write_waker: WakeSink,
}

impl Socket {
    pub(crate) fn new(
        transport: Arc<TransportBackend>, local_id: u32, remote_id: u32, acknowledged: usize,
    ) -> Self {
        let read_waker = WakeSink::new();
        let write_waker = WakeSink::new();

        Self {
            inner: SocketBackend {
                transport,
                local_id,

                span: info_span!(
                    "socket",
                    local_id,
                    remote_id = if remote_id != 0 { Some(remote_id) } else { None }
                ),

                read_waker: read_waker.source(),
                write_waker: write_waker.source(),

                state: State {
                    remote_id,
                    closed: false,

                    read_buffer: VecDeque::new(),

                    initial_acknowledgment: acknowledged as isize,
                    acknowledged: acknowledged as isize,
                    write_throttle: interval(Duration::from_millis(10)),
                }
                .into(),
            }
            .into(),

            read_waker,
            write_waker,
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        debug!(parent: &self.inner.span, "dropping socket");
        self.inner.transport.sockets.remove(&self.inner.local_id);
    }
}

impl AsyncRead for Socket {
    fn poll_read(
        mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let _span = self.inner.enter_span();
        self.read_waker.register(cx.waker());

        self.inner.check_transport_error()?;

        let mut state = self.inner.state.lock();
        let mut conn = self.inner.transport.get_connection();

        ready!(conn.poll_flush(cx)).map_err(io_error)?;

        let filled = buf.filled().len();
        while buf.remaining() > 0 {
            let Some(front) = state.read_buffer.front_mut() else {
                break;
            };

            buf.put(front.take(buf.remaining()));

            if !front.has_remaining() {
                state.read_buffer.pop_front();
            }
        }

        if state.closed {
            return Poll::Ready(Ok(()));
        }

        let filled = buf.filled().len() - filled;
        if filled == 0 {
            return Poll::Pending;
        }

        debug!(acknowledge = filled);

        conn.write_message(AdbMessage::new(
            AdbCommand::OKAY,
            self.inner.local_id,
            state.remote_id,
            (filled as u32).to_le_bytes().into(),
        ))
        .map_err(io_error)?;

        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Socket {
    fn poll_write(
        mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let _span = self.inner.enter_span();
        self.write_waker.register(cx.waker());

        self.inner.check_transport_error()?;

        let mut state = self.inner.state.lock();

        if state.closed {
            Err(io_error(ErrorKind::Closed))?;
        }

        let mut conn = self.inner.transport.get_connection();
        ready!(conn.poll_flush(cx)).map_err(io_error)?;

        if state.acknowledged <= -state.initial_acknowledgment {
            return Poll::Pending;
        }

        if state.acknowledged <= 0 {
            ready!(state.write_throttle.poll_tick(cx));
        }

        let len = buf
            .len()
            .min((state.acknowledged + state.initial_acknowledgment) as usize)
            .min(MAX_PAYLOAD as usize);

        state.acknowledged -= len as isize;
        debug!(write = len, acknowledged = state.acknowledged);

        conn.write_message(AdbMessage::new(
            AdbCommand::WRTE,
            self.inner.local_id,
            state.remote_id,
            buf[..len].to_vec(),
        ))
        .map_err(io_error)?;

        Poll::Ready(Ok(len))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _span = self.inner.enter_span();
        self.write_waker.register(cx.waker());

        self.inner.check_transport_error()?;

        let state = self.inner.state.lock();
        ready!(self.inner.transport.get_connection().poll_flush(cx)).map_err(io_error)?;

        if state.acknowledged == state.initial_acknowledgment {
            Poll::Ready(Ok(()))
        } else {
            if state.closed {
                Err(io_error((ErrorKind::msg("flush"), Error::from(ErrorKind::Closed))))?;
            }

            Poll::Pending
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _span = self.inner.enter_span();
        info!("shutdown");

        ready!(self.as_mut().poll_flush(cx))?;

        if let Some(msg) = self.inner.shutdown() {
            let mut conn = self.inner.transport.get_connection();
            conn.write_message(msg).map_err(io_error)?; // relies on self.poll_flush flushing connection
            conn.poll_flush(cx).map_err(io_error)
        } else {
            debug!("shutdown completed");
            Poll::Ready(Ok(()))
        }
    }
}
