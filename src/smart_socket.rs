use crate::connection::types::WeakConnection;
use crate::daemon::AdbDaemon;
use crate::util::{read_protocol_string, write_protocol_string};
use Status::*;
use adb_transport::Banner;
use derive_more::{Display, Error};
use eyre::{Result, WrapErr, bail};
use std::fmt::Write;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::{io, process};
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

enum DeviceSelector {
    Connection(Arc<WeakConnection>),
    Serial(String),
    Any,
    None,
}

#[derive(Debug, Display, Error)]
pub enum PickDeviceError {
    #[display("no device selected")]
    NotSelected,
    #[display("no devices/emulators found")]
    NoDevices,
    #[display("more than one device/emulator")]
    MoreThanOne,
    #[display("device '{_0}' not found")]
    NotFound(#[error(not(source))] String),
}

impl SmartSocket {
    pub fn pick_connection(&mut self) -> Result<Arc<WeakConnection>, PickDeviceError> {
        use DeviceSelector::*;

        let connections = &self.daemon.connections;
        let conn = match &self.device {
            Connection(conn) => return Ok(conn.clone()),
            Serial(serial) => connections
                .get(serial)
                .ok_or(PickDeviceError::NotFound(serial.clone()))?
                .downgrade(),
            Any => {
                let mut iter = connections.iter();
                let conn = iter.next().ok_or(PickDeviceError::NoDevices)?;
                if iter.next().is_some() {
                    return Err(PickDeviceError::MoreThanOne);
                }
                conn.downgrade()
            }
            None => Err(PickDeviceError::NotSelected)?,
        };
        self.device = Connection(conn.clone());
        Span::current().record("serial", &conn.serial);
        Ok(conn)
    }

    async fn consume_device_selector(&mut self, mut service: String) -> Result<String> {
        let mut legacy = true;
        if let Some(serial) = service.strip_prefix("host:transport:") {
            service = format!("host:tport:serial:{serial}")
        } else {
            legacy = false;
        };

        Ok(if let Some(s) = service.strip_prefix("host:tport:") {
            self.device = s
                .strip_prefix("serial:")
                .map(|s| DeviceSelector::Serial(s.into()))
                .unwrap_or(DeviceSelector::Any);

            let conn = self.pick_connection()?;

            self.respond(Okay).await?;
            if !legacy {
                self.conn.write_all(&conn.id.to_le_bytes()).await?;
            }

            let service = self.read_pstring().await.context("read service")?;
            trace!(service);

            service
        } else if let Some(s) = service.strip_prefix("host-serial:") {
            let Some((serial, service)) = s.split_once(":") else { bail!("malformed service") };

            self.device = DeviceSelector::Serial(serial.into());

            format!("host:{service}")
        } else if service.strip_prefix("host:").is_some() {
            self.device = DeviceSelector::Any;
            service
        } else {
            service
        })
    }

    async fn handle_service(&mut self) -> Result<()> {
        let service = self.read_pstring().await.context("read service")?;
        trace!(service);

        let service = self.consume_device_selector(service).await?;

        let Some(service) = service.strip_prefix("host:") else {
            let conn = self.pick_connection()?;
            self.daemon.forwardings.handle_reverse(&conn, &service)?;

            let mut socket =
                conn.upgrade()?.backend.open_socket(&service).await.context("open socket")?;
            self.respond(Okay).await?;

            let _ = tokio::io::copy_bidirectional_with_sizes(
                &mut socket,
                &mut self.conn,
                BUF_SIZE,
                BUF_SIZE,
            )
            .await;

            return Ok(());
        };

        match service {
            "version" => self.respond_data(Okay, "0029").await?,
            "is-adb-daemon-rs" => self.respond(Okay).await?,
            "kill" => {
                self.respond(Okay).await?;
                process::exit(0);
            }
            "features" => {
                let conn = self.pick_connection()?;
                self.respond_data(Okay, conn.upgrade()?.backend.banner()?.features_str()).await?
            }
            "devices" | "devices-l" => {
                let long = service.ends_with('l');

                let mut s = String::new();
                for device in self.daemon.connections.iter() {
                    if long {
                        let banner = device.backend.banner()?;

                        write!(s, "{:22}	device {}", device.serial, device.id)?;

                        for (name, key) in [
                            ("product", Banner::PRODUCT_NAME),
                            ("model", Banner::PRODUCT_MODEL),
                            ("device", Banner::PRODUCT_DEVICE),
                        ] {
                            let value = banner
                                .getprop(key)
                                .unwrap_or("Unknown")
                                .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
                            write!(s, " {name}:{value}")?;
                        }

                        writeln!(s, " transport_id:{}", device.id)?;
                    } else {
                        writeln!(s, "{}	device", device.key())?;
                    }
                }
                self.respond_data(Okay, s).await?
            }
            "get-state" => {
                self.pick_connection()?;
                self.respond_data(Okay, "device").await?
            }
            "wait-for-any-device" => {
                self.respond(Okay).await?;

                let mut updates = self.daemon.connections.device_updates();
                loop {
                    match self.pick_connection() {
                        Ok(_) => break,
                        Err(PickDeviceError::NoDevices | PickDeviceError::NotFound(_)) => {}
                        Err(err) => Err(err)?,
                    }

                    let mut buf = [0];
                    select! {
                        _ = updates.recv() => {},
                        res = self.conn.read(&mut buf) => {
                            if res? == 0 {
                                trace!("closed");
                                return Ok(());
                            } else {
                                bail!("should not read anything")
                            }
                        },
                    }
                }

                self.respond(Okay).await?;
            }
            "reconnect" => {
                self.pick_connection()?.upgrade()?.backend.close();
                self.respond_data(Okay, "").await?
            }
            "reconnect-offline" => {
                self.respond_data(Okay, "").await?;
            }
            s => {
                let daemon = self.daemon.clone();
                match s {
                    s if daemon.forwardings.handle_forward(self, s).await? => {}
                    _ => bail!("service not found"),
                }
            }
        }

        Ok(())
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
