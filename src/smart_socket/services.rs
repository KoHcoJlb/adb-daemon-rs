use crate::smart_socket::Status::Okay;
use crate::smart_socket::devices::PickDeviceError;
use crate::smart_socket::{BUF_SIZE, SmartSocket};
use adb_transport::DeviceType;
use eyre::{Context, bail, eyre};
use futures::FutureExt;
use std::pin::pin;
use std::process;
use time::ext::NumericalStdDuration;
use tokio::io::{AsyncWriteExt, BufReader, split};
use tokio::select;
use tokio::time::timeout;
use tracing::{error, trace};

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

            // Emulate half-close on tcp side, so client can read buffered data after the socket is closed
            // Otherwise with copy_bidirectional_with_sizes, if client was actively writing, it will receive
            // write error and exit
            let (socket_r, mut socket_w) = split(&mut socket);
            let (conn_r, mut conn_w) = split(&mut self.conn);

            let mut conn_r = BufReader::with_capacity(BUF_SIZE, conn_r);

            {
                let mut socket_to_conn = pin!(
                    async {
                        let mut r = BufReader::with_capacity(BUF_SIZE, socket_r);
                        tokio::io::copy_buf(&mut r, &mut conn_w).await?;
                        trace!("socket_to_conn ended");
                        conn_w.shutdown().await
                    }
                    .fuse()
                );
                let mut conn_to_socket = pin!(
                    async {
                        tokio::io::copy_buf(&mut conn_r, &mut socket_w).await?;
                        trace!("conn_to_socket ended");
                        socket_w.shutdown().await
                    }
                    .fuse()
                );

                loop {
                    select! {
                        _ = socket_to_conn.as_mut() => break,
                        _ = conn_to_socket.as_mut() => {}
                    }
                }
            }

            // Try read from TCP stream to the end, otherwise it can RST when TcpStream is dropped
            // instead of sending FIN to the `shutdown` earlier
            // During tests simple sleep for a couple of seconds here was sufficient,
            // but after reading about close(2), SO_LINGER and such this seems slightly more correct
            if let Err(err) =
                timeout(5.std_seconds(), tokio::io::copy(&mut conn_r, &mut tokio::io::sink()))
                    .await
                    .map_or(Err(eyre!("timeout")), |res| res.map_err(|e| eyre!(e)))
            {
                error!(?err, "read to end");
            }

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
