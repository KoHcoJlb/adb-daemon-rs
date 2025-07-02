use crate::connection::types::WeakConnection;
use crate::smart_socket::Status::Okay;
use crate::smart_socket::{DeviceSelector, SmartSocket};
use adb_transport::Banner;
use derive_more::{Display, Error};
use eyre::Result;
use eyre::{Context, bail};
use std::fmt::Write;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::{Span, trace};

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
        use crate::smart_socket::DeviceSelector::*;

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

    pub(super) async fn consume_device_selector(&mut self, mut service: String) -> Result<String> {
        let legacy = if let Some(serial) = service.strip_prefix("host:transport:") {
            service = format!("host:tport:serial:{serial}");
            true
        } else {
            false
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

    pub(super) fn list_devices(&self, long: bool) -> Result<String> {
        let mut s = String::new();
        for device in self.daemon.connections.iter() {
            if long {
                let banner = device.backend.banner()?;

                write!(s, "{:22}	{} {}", device.serial, device.banner.device_type, device.id)?;

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
                writeln!(s, "{}	{}", device.key(), device.banner.device_type)?;
            }
        }
        Ok(s)
    }
}
