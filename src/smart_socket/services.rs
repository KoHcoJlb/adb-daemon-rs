use crate::smart_socket::Status::Okay;
use crate::smart_socket::devices::PickDeviceError;
use crate::smart_socket::{BUF_SIZE, SmartSocket};
use adb_transport::DeviceType;
use eyre::{Context, bail};
use std::process;
use tracing::trace;

impl SmartSocket {
    pub(super) async fn handle_service(&mut self) -> eyre::Result<()> {
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
                self.respond_data(Okay, self.list_devices(long)?).await?
            }
            "track-devices" | "track-devices-l" => {
                self.respond(Okay).await?;

                let long = service.ends_with('l');
                self.write_pstring(self.list_devices(long)?).await?;

                let mut updates = self.daemon.connections.device_updates();
                while self.wait_for(updates.recv()).await?.is_some() {
                    self.write_pstring(self.list_devices(long)?).await?;
                }
            }
            "get-state" => {
                self.pick_connection()?;
                self.respond_data(Okay, "device").await?
            }
            "wait-for-any-device" => {
                self.respond(Okay).await?;

                let mut updates = self.daemon.connections.device_updates();
                let selector = self.device.clone();
                loop {
                    self.device = selector.clone();
                    match self.pick_connection() {
                        Ok(conn) if conn.banner.device_type == DeviceType::Device => break,
                        Ok(_) | Err(PickDeviceError::NoDevices | PickDeviceError::NotFound(_)) => {}
                        Err(err) => Err(err)?,
                    }

                    if self.wait_for(updates.recv()).await?.is_none() {
                        break;
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
}
