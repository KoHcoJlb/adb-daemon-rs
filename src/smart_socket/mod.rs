mod devices;
mod services;

use crate::connection::types::WeakConnection;
use crate::daemon::AdbDaemon;
use crate::util::{read_protocol_string, write_protocol_string};
use Status::*;
use eyre::{Report, Result, bail};
use std::io;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::select;
use tracing::{Span, error, instrument, trace};

pub const BUF_SIZE: usize = 1024 * 1024;

pub enum Status {
    Okay,
    Fail,
}

impl Deref for Status {
    type Target = &'static [u8; 4];

    fn deref(&self) -> &Self::Target {
        match self {
            Okay => &b"OKAY",
            Fail => &b"FAIL",
        }
    }
}

pub struct SmartSocket {
    conn: TcpStream,
    daemon: Arc<AdbDaemon>,
    device: DeviceSelector,
}

impl SmartSocket {
    #[inline]
    pub async fn read_pstring(&mut self) -> Result<String> {
        read_protocol_string(&mut self.conn).await
    }

    #[inline]
    pub async fn write_pstring(&mut self, data: impl AsRef<[u8]>) -> Result<()> {
        write_protocol_string(&mut self.conn, data).await
    }

    #[inline]
    pub async fn respond(&mut self, status: Status) -> Result<()> {
        self.conn.write_all(*status).await.map_err(Into::into)
    }

    #[inline]
    pub async fn respond_data(&mut self, status: Status, data: impl AsRef<[u8]>) -> Result<()> {
        self.respond(status).await?;
        self.write_pstring(data).await?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum DeviceSelector {
    Connection(Arc<WeakConnection>),
    Serial(String),
    Any,
    None,
}

impl SmartSocket {
    async fn wait_for<T, E>(
        &mut self, event: impl Future<Output = Result<T, E>>,
    ) -> Result<Option<T>>
    where
        Report: From<E>,
    {
        let mut buf = [0];
        select! {
            // res = event => res.map(Some).map_err(|e| e.into()),
            res = event => Ok(Some(res?)),
            res = self.conn.read(&mut buf) => {
                if res? == 0 {
                    trace!("closed");
                    Ok(None)
                } else {
                    bail!("should not read anything")
                }
            },
        }
    }

    #[instrument(name = "smart_socket", skip_all, fields(id, serial))]
    pub async fn run(daemon: Arc<AdbDaemon>, socket: TcpStream) {
        static CONN_ID: AtomicU32 = AtomicU32::new(0);
        Span::current().record("id", CONN_ID.fetch_add(1, Ordering::SeqCst));

        let mut this = SmartSocket { daemon, conn: socket, device: DeviceSelector::None };

        if let Err(err) = this.handle_service().await {
            if let Some(err) = err.downcast_ref::<io::Error>() {
                if err.kind() == io::ErrorKind::UnexpectedEof {
                    return;
                }
            }

            error!(?err, "handle connection");
            let _ = this.respond_data(Fail, err.to_string()).await;
        }
    }
}
