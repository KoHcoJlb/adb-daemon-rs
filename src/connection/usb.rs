use crate::config::config;
use crate::connection::manager::ConnectionMgr;
use crate::connection::{ConnectionBackend, WeakConnection};
use crate::forward::ForwardingMgr;
use adb_transport::DeviceType;
use adb_transport::connection::usb::UsbConnection;
use eyre::{Context, Result};
use nusb::DeviceInfo;
use std::sync::Arc;
use tokio::spawn;
use tracing::{error, instrument, warn};

async fn accept_sockets_task(connection: Arc<WeakConnection>, forwarding_mgr: Arc<ForwardingMgr>) {
    loop {
        let sock = {
            let Ok(conn) = connection.upgrade() else {
                return;
            };
            let ConnectionBackend::AProto(transport) = &conn.backend;

            match transport.accept_socket().await {
                Ok(s) => s,
                Err(err) => {
                    warn!(?err, "accept socket");
                    return;
                }
            }
        };

        forwarding_mgr.handle_socket(&connection, sock.into()).await;
    }
}

impl ConnectionMgr {
    #[instrument(skip(self, device))]
    async fn create_usb_connection(self: Arc<Self>, serial: String, device: DeviceInfo) {
        use adb_transport::connection::usb::UsbError;
        use adb_transport::{AuthMode, Error, ErrorKind};
        use nusb::transfer::TransferError;

        if let Err(err) = async {
            let transport_conn = match UsbConnection::new(&device) {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(?err, "create usb connection");
                    return Ok(());
                }
            };

            // create usb transport err=Error { msg: "create transport", source: Error { kind: TransportError, source: Some(Error { kind: Usb(Transfer(Fault)), source: None }) } }

            let mut transport = match adb_transport::Transport::new(transport_conn.into())
                .await
                .context("create transport")
            {
                Ok(t) => t,
                Err(err) => {
                    if let Some(Error {
                        kind: ErrorKind::Usb(UsbError::Transfer(TransferError::Fault)),
                        ..
                    }) = err.root_cause().downcast_ref::<Error>()
                    {
                        warn!("transfer fault, reset device");
                        if let Err(err) = device.open().and_then(|d| d.reset()) {
                            warn!(?err, "reset failed");
                        }
                        return Ok(());
                    }
                    return Err(err);
                }
            };

            match transport.auth(AuthMode::Existing(&self.privkey)).await {
                Ok(_) => {}
                Err(Error { kind: ErrorKind::AuthRejected, .. }) => {
                    warn!("auth rejected, offering key");

                    transport.auth(AuthMode::New(&self.privkey)).await.context("offer key")?;
                }
                Err(err) => Err(err).context("auth error")?,
            }

            if transport.banner()?.device_type == DeviceType::Device {
                let conn = self.new_connection(serial, ConnectionBackend::AProto(transport))?;
                spawn(accept_sockets_task(conn.downgrade(), self.forwardings.clone()));
            } else {
                warn!("not a device");
                transport.close();
            }

            <Result<_>>::Ok(())
        }
        .await
        {
            error!(?err, "create usb transport");
        }
    }

    pub(super) async fn refresh_usb(self: &Arc<Self>) -> Result<()> {
        for device in nusb::list_devices().context("list devices")? {
            if !UsbConnection::is_supported(&device) {
                continue;
            }

            let Some(serial) = device.serial_number() else {
                continue;
            };

            if !config().usb.is_whitelisted(serial) {
                continue;
            }

            if self.by_serial.contains_key(serial) {
                continue;
            }

            let serial = serial.to_string();
            spawn(self.clone().create_usb_connection(serial, device));
        }
        Ok(())
    }
}
