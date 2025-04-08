use crate::config::config;
use crate::connection::manager::ConnectionMgr;
use crate::connection::types::{ConnectionBackend, MaybeConnection, WeakConnection};
use crate::forward::ForwardingMgr;
use adb_transport::ErrorKind;
use adb_transport::connection::usb::{UsbConnection, UsbError};
use eyre::{Context, Result};
use nusb::DeviceInfo;
use std::sync::Arc;
use tokio::spawn;
use tracing::{Instrument, error, warn};

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
    async fn create_usb_connection(
        self: Arc<Self>, daemon_conn: MaybeConnection, transport_conn: UsbConnection,
        device: DeviceInfo,
    ) {
        use adb_transport::connection::usb::UsbError;
        use adb_transport::{AuthMode, Error, ErrorKind};
        use nusb::transfer::TransferError;

        if let Err(err) = async {
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

            let conn = self.new_connection(daemon_conn, ConnectionBackend::AProto(transport))?;
            spawn(
                accept_sockets_task(conn.downgrade(), self.forwardings.clone())
                    .instrument(conn.span.clone()),
            );

            <Result<_>>::Ok(())
        }
        .await
        {
            error!(?err, "create usb transport");
        }
    }

    pub(super) async fn refresh_usb(self: &Arc<Self>) -> Result<()> {
        for device in nusb::list_devices().context("list devices")? {
            let Some(serial) = device.serial_number() else {
                continue;
            };

            if !config().usb.is_whitelisted(serial) {
                continue;
            }

            if self.by_serial.contains_key(serial) {
                continue;
            }

            let transport_conn = match UsbConnection::new(&device) {
                Ok(conn) => conn,
                Err(adb_transport::Error {
                    kind: ErrorKind::Usb(UsbError::Unsupported), ..
                }) => continue,
                Err(err) => {
                    warn!(serial, ?err, "create usb connection");
                    continue;
                }
            };

            let daemon_conn = MaybeConnection::alloc(serial);
            let span = daemon_conn.span.clone();
            spawn(
                self.clone()
                    .create_usb_connection(daemon_conn, transport_conn, device)
                    .instrument(span),
            );
        }
        Ok(())
    }
}
