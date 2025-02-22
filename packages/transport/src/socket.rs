use crate::error::ErrorKind;
use crate::message::{AdbCommand, AdbMessage};
use crate::transport::{TransportBackend, MAX_PAYLOAD};
use crate::util::MaybeDone;
use crate::{Error, Result};
use diatomic_waker::DiatomicWaker;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::future::Future;
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
use tracing::{debug, info, info_span, warn, Instrument, Span};

pub(crate) struct State {
    pub remote_id: u32,
    pub closed: bool,

    read_buffer: VecDeque<Cursor<Vec<u8>>>,
    read_fut: MaybeDone<io::Result<()>>,

    initial_acknowledgment: isize,
    acknowledged: isize,
    write_fut: MaybeDone<io::Result<()>>,
    write_throttle: Interval,
}

pub(crate) struct SocketBackend {
    transport: Arc<TransportBackend>,
    local_id: u32,
    span: Span,

    pub read_waker: DiatomicWaker,
    pub write_waker: DiatomicWaker,

    pub state: Mutex<State>,
}

impl SocketBackend {
    fn check_transport_error(&self) -> io::Result<()> {
        if let Some(err) = self.transport.error.get() {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, err.clone()))
        } else {
            Ok(())
        }
    }

    fn enter_span(&self) -> EnteredSpan {
        self.span.clone().entered()
    }

    fn poll_flush(&self, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe { self.write_waker.register(cx.waker()) };

        self.check_transport_error()?;

        let mut state = self.state.lock();
        ready!(state.write_fut.poll(cx))?;

        if state.acknowledged == state.initial_acknowledgment {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn shutdown(&self) -> Option<impl Future<Output = ()>> {
        if self.check_transport_error().is_ok() {
            let mut state = self.state.lock();
            if !state.closed {
                state.closed = true;

                let transport = self.transport.clone();
                let msg =
                    AdbMessage::new(AdbCommand::CLSE, self.local_id, state.remote_id, Vec::new());
                return Some(
                    async move {
                        if let Err(err) = transport.write_message(msg).await {
                            warn!(?err, "failed to close socket");
                        }
                    }
                    .instrument(self.span.clone()),
                );
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

        if let Some(fut) = self.shutdown() {
            warn!("dropping non closed socket");
            spawn(fut);
        } else {
            debug!("dropping socket backend");
        }
    }
}

pub struct Socket {
    pub(crate) inner: Arc<SocketBackend>,
}

impl Socket {
    pub(crate) fn new(
        transport: Arc<TransportBackend>, local_id: u32, remote_id: u32, acknowledged: usize,
    ) -> Self {
        Self {
            inner: SocketBackend {
                transport,
                local_id,

                span: info_span!(
                    "socket",
                    local_id,
                    remote_id = if remote_id != 0 { Some(remote_id) } else { None }
                ),

                read_waker: DiatomicWaker::new(),
                write_waker: DiatomicWaker::new(),

                state: State {
                    remote_id,
                    closed: false,

                    read_fut: MaybeDone::empty(),
                    read_buffer: VecDeque::new(),

                    initial_acknowledgment: acknowledged as isize,
                    acknowledged: acknowledged as isize,
                    write_fut: MaybeDone::empty(),
                    write_throttle: interval(Duration::from_millis(10)),
                }
                .into(),
            }
            .into(),
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
        self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        unsafe { self.inner.read_waker.register(cx.waker()) }

        let _span = self.inner.enter_span();
        self.inner.check_transport_error()?;

        let mut state = self.inner.state.lock();

        ready!(state.read_fut.poll(cx))?;

        let filled = buf.filled().len();
        while buf.remaining() > 0 {
            let Some(front) = state.read_buffer.front_mut() else {
                break;
            };

            let _ = Pin::new(&mut *front).poll_read(cx, buf);

            if front.position() == front.get_ref().len() as u64 {
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

        let msg = AdbMessage::new(
            AdbCommand::OKAY,
            self.inner.local_id,
            state.remote_id,
            (filled as u32).to_le_bytes().into(),
        );

        let transport = self.inner.transport.clone();
        let _ = state
            .read_fut
            .set(async move {
                transport
                    .write_message(msg)
                    .await
                    .map_err(|e| io::Error::other(Error::from((ErrorKind::msg("OKAY"), e))))
            })
            .poll(cx)?;

        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Socket {
    fn poll_write(
        self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        unsafe { self.inner.write_waker.register(cx.waker()) }

        let _span = self.inner.enter_span();
        self.inner.check_transport_error()?;

        let mut state = self.inner.state.lock();

        if state.closed {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, Error::from(ErrorKind::Closed)))?;
        }

        ready!(state.write_fut.poll(cx))?;

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

        let msg = AdbMessage::new(
            AdbCommand::WRTE,
            self.inner.local_id,
            state.remote_id,
            buf[..len].to_vec(),
        );

        let transport = self.inner.transport.clone();
        let _ = state
            .write_fut
            .set(async move {
                transport
                    .write_message(msg)
                    .await
                    .map_err(|e| io::Error::other(Error::from((ErrorKind::msg("WRTE"), e))))
            })
            .poll(cx)?;

        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _span = self.inner.enter_span();
        self.inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _span = self.inner.enter_span();
        info!("shutdown");

        ready!(self.inner.poll_flush(cx))?;

        if let Some(fut) = self.inner.shutdown() {
            self.inner
                .state
                .lock()
                .write_fut
                .set(async move {
                    fut.await;
                    Ok(())
                })
                .poll(cx)
        } else {
            debug!("shutdown completed");
            Poll::Ready(Ok(()))
        }
    }
}
