use crate::auth::encode_public_key;
use crate::banner::Banner;
use crate::connection::{Connection, ConnectionTrait};
use crate::error::ErrorKind;
use crate::message::{AdbCommand, AdbMessage};
use crate::socket::{Socket, SocketBackend};
use crate::{Error, Result};
use dashmap::DashMap;
use flume::{bounded, Receiver, Sender};
use parking_lot::{Mutex, MutexGuard};
use rsa::{Pkcs1v15Sign, RsaPrivateKey};
use sha1::Sha1;
use std::future::{poll_fn, Future};
use std::mem::take;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{ready, Context, Poll, Wake, Waker};
use tokio::select;
use tokio::sync::Notify;
use tracing::{debug, error, info, trace, warn, Instrument, Span};

pub(crate) const DELAYED_ACK_BYTES: u32 = 32 * 1024 * 1024;
pub(crate) const MAX_PAYLOAD: u32 = 1024 * 1024;
const VERSION: u32 = 0x01000001;

const AUTH_TYPE_SIGNATURE: u32 = 2;
const AUTH_TYPE_PUBLICKEY: u32 = 3;

const CONN_STR: &str = "host::features=shell_v2,cmd,stat_v2,ls_v2,fixed_push_mkdir,apex,abb,fixed_push_symlink_timestamp,abb_exec,remount_shell,track_app,sendrecv_v2,sendrecv_v2_brotli,sendrecv_v2_lz4,sendrecv_v2_zstd,sendrecv_v2_dry_run_send,openscreen_mdns,devicetracker_proto_format,delayed_ack";

#[derive(Default)]
struct WakerCollection(Mutex<Vec<Waker>>);

impl Wake for WakerCollection {
    fn wake(self: Arc<Self>) {
        for waker in take(&mut *self.0.lock()) {
            waker.wake();
        }
    }
}

struct ConnectionWrapper {
    inner: Connection,
    write_wakers: Arc<WakerCollection>,
}

pub struct ConnectionWrapperGuard<'a> {
    inner: MutexGuard<'a, Option<ConnectionWrapper>>,
    transport: &'a TransportBackend,
}

impl ConnectionWrapperGuard<'_> {
    fn get(&mut self) -> Result<&mut ConnectionWrapper> {
        self.inner.as_mut().ok_or(ErrorKind::Closed.into())
    }

    fn on_error(&mut self, err: Error) -> Error {
        MutexGuard::unlocked(&mut self.inner, || self.transport.on_error(err))
    }
}

impl ConnectionTrait for ConnectionWrapperGuard<'_> {
    fn poll_read_message(&mut self, cx: &mut Context) -> Poll<Result<AdbMessage>> {
        self.get()?.inner.poll_read_message(cx)
    }

    fn write_message(&mut self, msg: AdbMessage) -> Result<()> {
        trace!(?msg, "write message");
        self.get()?
            .inner
            .write_message(msg)
            .map_err(|err| self.on_error((ErrorKind::msg("write"), err).into()))
    }

    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<()>> {
        let inner = self.get()?;
        inner.write_wakers.0.lock().push(cx.waker().clone());
        inner
            .inner
            .poll_flush(&mut Context::from_waker(&inner.write_wakers.clone().into()))
            .map_err(|err| self.on_error((ErrorKind::msg("write (flush)"), err).into()))
    }
}

pub trait ConnectionExt: Sized {
    fn get_connection(&mut self) -> impl ConnectionTrait;

    fn read_message_async(mut self) -> impl Future<Output = Result<AdbMessage>> {
        poll_fn(move |cx| self.get_connection().poll_read_message(cx))
    }

    fn write_message_async(mut self, msg: AdbMessage) -> impl Future<Output = Result<()>> {
        let mut msg = Some(msg);
        poll_fn(move |cx| {
            let mut conn = self.get_connection();
            loop {
                ready!(conn.poll_flush(cx))?;
                if let Some(msg) = msg.take() {
                    conn.write_message(msg)?;
                } else {
                    return Poll::Ready(Ok(()));
                }
            }
        })
    }
}

impl ConnectionExt for &mut Connection {
    fn get_connection(&mut self) -> impl ConnectionTrait {
        self
    }
}

pub(crate) struct TransportBackend {
    connection: Mutex<Option<ConnectionWrapper>>,
    last_id: AtomicU32,
    pub error: OnceLock<Arc<Error>>,
    close_notify: Notify,
    pub sockets: DashMap<u32, Arc<SocketBackend>>,
    banner: Banner,
}

impl ConnectionExt for &TransportBackend {
    fn get_connection(&mut self) -> impl ConnectionTrait {
        (&*self).get_connection()
    }
}

impl TransportBackend {
    async fn handle_message(self: &Arc<Self>, pending: &Sender<PendingSocket>) -> Result<()> {
        self.check_error()?;

        let mut msg = select! {
            msg = self.read_message_async() => msg?,
            _ = self.close_notify.notified() => return Ok(()),
        };
        trace!(?msg, "received message");

        match msg.header.command {
            AdbCommand::OPEN => {
                if msg.payload.last() == Some(&0) {
                    msg.payload.pop();
                }

                let socket = PendingSocket {
                    backend: self.clone(),
                    remote_id: msg.header.arg0,
                    acknowledged: msg.header.arg1 as usize,
                    service: String::from_utf8(msg.payload)
                        .map_err(|_| (ErrorKind::InvalidData, "service is not utf8"))?,
                };

                if let Err(err) = pending.send_async(socket).await {
                    err.into_inner().close().await?;
                }
            }
            AdbCommand::OKAY | AdbCommand::WRTE | AdbCommand::CLSE => {
                let local_id = msg.header.arg1;
                let closed = if let Some(socket) = self.sockets.get(&local_id) {
                    socket.handle_message(msg)?
                } else {
                    debug!(local_id, "socket not found");
                    false
                };
                if closed {
                    self.sockets.remove(&local_id);
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn reader_task(self: Arc<Self>, pending: Sender<PendingSocket>) {
        loop {
            if let Err(err) = self.handle_message(&pending).await {
                error!(?err, "read message error");
                self.on_error(err);
                break;
            }
        }
    }

    fn on_error(&self, err: Error) -> Error {
        let err = Arc::new(err);

        if self.error.set(err.clone()).is_ok() {
            warn!(?err, "set transport error");
            self.connection.lock().take();
            self.close_notify.notify_waiters();
            self.sockets.retain(|_, sock| {
                sock.read_waker.notify();
                sock.write_waker.notify();
                false
            });
        };

        ErrorKind::TransportError(err).into()
    }

    fn check_error(&self) -> Result<()> {
        if let Some(err) = self.error.get() {
            Err(ErrorKind::TransportError(err.clone()).into())
        } else {
            Ok(())
        }
    }

    pub fn get_connection(&self) -> ConnectionWrapperGuard<'_> {
        ConnectionWrapperGuard { inner: self.connection.lock(), transport: self }
    }
}

impl Drop for TransportBackend {
    fn drop(&mut self) {
        info!("dropped transport backend");
    }
}

pub struct PendingSocket {
    backend: Arc<TransportBackend>,
    remote_id: u32,
    acknowledged: usize,
    service: String,
}

impl PendingSocket {
    pub fn service(&self) -> &str {
        &self.service
    }

    pub async fn accept(self) -> Result<Socket> {
        let local_id = self.backend.last_id.fetch_add(1, Ordering::SeqCst);

        let socket = Socket::new(self.backend.clone(), local_id, self.remote_id, self.acknowledged);
        self.backend.sockets.insert(local_id, socket.inner.clone());

        let msg = AdbMessage::new(
            AdbCommand::OKAY,
            local_id,
            self.remote_id,
            DELAYED_ACK_BYTES.to_le_bytes().into(),
        );
        self.backend.write_message_async(msg).await?;

        Ok(socket)
    }

    pub async fn close(self) -> Result<()> {
        self.backend
            .write_message_async(AdbMessage::new(AdbCommand::CLSE, 0, self.remote_id, Vec::new()))
            .await
    }
}

pub enum AuthMode<'a> {
    New(&'a RsaPrivateKey),
    Existing(&'a RsaPrivateKey),
}

pub struct AuthTransport {
    conn: Connection,
    last_auth: Option<AdbMessage>,
}

impl AuthTransport {
    pub async fn new(mut conn: Connection) -> Result<Self> {
        let cnxn =
            AdbMessage::new(AdbCommand::CNXN, VERSION, MAX_PAYLOAD, CONN_STR.as_bytes().into());
        conn.write_message_async(cnxn).await?;
        Ok(Self { conn, last_auth: None })
    }

    fn handle_cnxn(self, msg: AdbMessage) -> Result<Transport> {
        let conn_str = String::from_utf8(msg.payload)
            .map_err(|_| (ErrorKind::InvalidData, "malformed conn str"))?;

        debug!(conn_str);
        let banner =
            Banner::from_str(&conn_str).map_err(|e| (ErrorKind::Msg("parse banner".into()), e))?;

        if !banner.features.contains("delayed_ack") {
            Err(ErrorKind::DelayedAckNotAvailable)?
        }

        let inner = Arc::new(TransportBackend {
            connection: Some(ConnectionWrapper {
                inner: self.conn,
                write_wakers: Default::default(),
            })
            .into(),
            last_id: AtomicU32::new(1),
            error: OnceLock::new(),
            close_notify: Notify::new(),
            sockets: DashMap::new(),
            banner,
        });

        let (pending_tx, pending_rx) = bounded(3);
        tokio::spawn(inner.clone().reader_task(pending_tx).instrument(Span::current()));

        Ok(Transport { inner, pending_rx: Some(pending_rx) })
    }

    pub async fn auth(mut self, key: AuthMode<'_>) -> Result<Result<Transport, Self>> {
        let mut key = Some(key);
        loop {
            let msg = if let Some(msg) = self.last_auth.take() {
                msg
            } else {
                self.conn.read_message_async().await?
            };
            trace!(?msg);

            match msg.header.command {
                AdbCommand::AUTH => {
                    let msg = match key.take() {
                        None => {
                            self.last_auth = Some(msg);
                            return Ok(Err(self));
                        }
                        Some(AuthMode::Existing(key)) => {
                            let signature = key
                                .sign(Pkcs1v15Sign::new::<Sha1>(), &msg.payload)
                                .map_err(|e| (ErrorKind::Sign, e))?;
                            AdbMessage::new(AdbCommand::AUTH, AUTH_TYPE_SIGNATURE, 0, signature)
                        }
                        Some(AuthMode::New(key)) => AdbMessage::new(
                            AdbCommand::AUTH,
                            AUTH_TYPE_PUBLICKEY,
                            0,
                            encode_public_key(&key.to_public_key())?.into_bytes(),
                        ),
                    };

                    self.conn.write_message_async(msg).await?;
                }
                AdbCommand::CNXN => {
                    return self.handle_cnxn(msg).map(Ok);
                }
                _ => {}
            }
        }
    }
}

pub struct Transport {
    inner: Arc<TransportBackend>,
    pending_rx: Option<Receiver<PendingSocket>>,
}

impl Transport {
    fn check_error(&self) -> Result<()> {
        self.inner.check_error()
    }
}

impl Transport {
    pub fn close(&self) {
        self.inner.on_error(ErrorKind::Closed.into());
    }

    pub fn is_closed(&self) -> bool {
        self.inner.error.get().is_some()
    }

    pub fn banner(&self) -> &Banner {
        &self.inner.banner
    }

    pub async fn open_socket(&self, service: &str) -> Result<Socket> {
        self.check_error()?;

        info!(socket_count = self.inner.sockets.len());

        let local_id = self.inner.last_id.fetch_add(1, Ordering::SeqCst);
        info!(local_id, service, "open socket");

        let mut socket = Socket::new(self.inner.clone(), local_id, 0, 0);
        self.inner.sockets.insert(local_id, socket.inner.clone());

        let mut payload = service.as_bytes().to_vec();
        payload.push(0);

        self.inner
            .write_message_async(AdbMessage::new(
                AdbCommand::OPEN,
                local_id,
                DELAYED_ACK_BYTES,
                payload,
            ))
            .await?;

        let remote_id = poll_fn::<Result<_>, _>(|cx| {
            self.check_error()?;

            socket.write_waker.register(cx.waker());

            let state = socket.inner.state.lock();
            if state.remote_id != 0 {
                return Poll::Ready(Ok(state.remote_id));
            }
            if state.closed {
                Err(ErrorKind::Closed)?;
            }
            Poll::Pending
        })
        .await?;

        info!(local_id, remote_id, "opened socket");

        Ok(socket)
    }

    pub fn drop_inbound(&mut self) {
        self.pending_rx.take();
    }

    pub async fn accept_socket(&self) -> Result<PendingSocket> {
        self.pending_rx
            .as_ref()
            .ok_or((ErrorKind::Closed, "no receiver"))?
            .recv_async()
            .await
            .map_err(|_| ErrorKind::Closed.into())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        info!("dropped transport")
    }
}
