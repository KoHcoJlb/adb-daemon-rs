use crate::forward::ReverseForwards;
use adb_transport::Banner;
use delegate::delegate;
use derive_more::From;
use eyre::Result;
use std::borrow::Cow;
use std::io;
use std::ops::Deref;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::debug;

pub mod manager;
mod usb;

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
    pub fn service(&self) -> Cow<str> {
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
            ConnectionBackend::AProto(t) => t.banner().map_err(Into::into),
        }
    }

    pub async fn open_socket(&self, service: &str) -> Result<Socket> {
        Ok(match self {
            ConnectionBackend::AProto(conn) => conn.open_socket(service).await?.into(),
        })
    }
}

pub struct Connection {
    pub id: u64,
    pub serial: String,
    pub backend: ConnectionBackend,
    pub reverse_forwards: ReverseForwards,
}

impl Deref for Connection {
    type Target = ConnectionBackend;

    fn deref(&self) -> &Self::Target {
        &self.backend
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        debug!(id = self.id, serial = self.serial, "dropped connection");
    }
}
