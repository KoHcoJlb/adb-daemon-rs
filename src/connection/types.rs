use crate::forward::ReverseForwards;
use adb_transport::Banner;
use delegate::delegate;
use derive_more::Debug;
use derive_more::{Display, Error, From};
use eyre::Result;
use std::borrow::Cow;
use std::io;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::{Span, debug, info_span};

#[derive(From)]
pub enum Socket {
    AProto(adb_transport::Socket),
}

impl AsyncRead for Socket {
    delegate! {
        to match self.get_mut() {
            Socket::AProto(s) => Pin::new(s),
        } {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>>;
        }
    }
}

impl AsyncWrite for Socket {
    delegate! {
        to match self.get_mut() {
            Socket::AProto(s) => Pin::new(s),
        } {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<Result<usize, io::Error>>;
            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>>;
            fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>>;
        }
    }
}

#[derive(From)]
pub enum PendingSocket {
    AProto(adb_transport::PendingSocket),
}

impl PendingSocket {
    pub fn service(&self) -> Cow<'_, str> {
        match self {
            PendingSocket::AProto(s) => Cow::Borrowed(s.service()),
        }
    }

    pub async fn accept(self) -> Result<Socket> {
        Ok(match self {
            PendingSocket::AProto(s) => s.accept().await?.into(),
        })
    }

    pub async fn close(self) -> Result<()> {
        match self {
            PendingSocket::AProto(s) => s.close().await?,
        }
        Ok(())
    }
}

pub enum ConnectionBackend {
    AProto(adb_transport::Transport),
}

impl ConnectionBackend {
    pub fn close(&self) {
        match self {
            ConnectionBackend::AProto(c) => c.close(),
        }
    }

    pub fn is_closed(&self) -> bool {
        match self {
            ConnectionBackend::AProto(t) => t.is_closed(),
        }
    }

    pub fn banner(&self) -> Result<&Banner> {
        match self {
            ConnectionBackend::AProto(t) => Ok(t.banner()),
        }
    }

    pub async fn open_socket(&self, service: &str) -> Result<Socket> {
        Ok(match self {
            ConnectionBackend::AProto(conn) => conn.open_socket(service).await?.into(),
        })
    }
}

pub(super) struct MaybeConnection {
    id: u64,
    serial: String,
    pub span: Span,
}

impl MaybeConnection {
    pub fn alloc(serial: &str) -> Self {
        static ID: AtomicU64 = AtomicU64::new(1);
        let id = ID.fetch_add(1, Ordering::SeqCst);
        let span = info_span!(parent: None, "connection", id, serial);
        Self { id, serial: serial.into(), span }
    }

    pub fn create(self, backend: ConnectionBackend) -> Result<Arc<Connection>> {
        let banner = backend.banner()?.clone();
        Ok(Arc::new_cyclic(|this| Connection {
            weak: Arc::new(WeakConnection {
                strong: this.clone(),
                id: self.id,
                serial: self.serial,
                banner,
                reverse_forwards: Default::default(),
                span: self.span,
            }),
            backend,
        }))
    }
}

#[derive(Debug, Display, Error)]
#[display("device disconnected")]
pub struct Disconnected;

#[derive(Debug)]
pub struct WeakConnection {
    #[debug(skip)]
    strong: Weak<Connection>,
    pub id: u64,
    pub serial: String,
    #[debug(skip)]
    pub banner: Banner,
    #[debug(skip)]
    pub reverse_forwards: ReverseForwards,
    #[debug(skip)]
    pub span: Span,
}

impl WeakConnection {
    pub fn upgrade(&self) -> Result<Arc<Connection>, Disconnected> {
        self.strong.upgrade().ok_or(Disconnected)
    }

    pub fn is_closed(&self) -> bool {
        self.upgrade().ok().is_none_or(|c| c.backend.is_closed())
    }
}

impl Drop for WeakConnection {
    fn drop(&mut self) {
        debug!(id = self.id, serial = self.serial, "dropped weak connection");
    }
}

pub struct Connection {
    pub weak: Arc<WeakConnection>,
    pub backend: ConnectionBackend,
}

impl Connection {
    pub fn downgrade(&self) -> Arc<WeakConnection> {
        self.weak.clone()
    }
}

impl Deref for Connection {
    type Target = WeakConnection;

    fn deref(&self) -> &Self::Target {
        &self.weak
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        debug!(id = self.id, serial = self.serial, "dropped connection");
    }
}
