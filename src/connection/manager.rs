use crate::config::config;
use crate::connection::{Connection, ConnectionBackend};
use crate::forward::ForwardingMgr;
use dashmap::DashMap;
use dashmap::mapref::multiple::RefMulti;
use dashmap::mapref::one::Ref;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use rsa::RsaPrivateKey;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::futures::Notified;
use tokio::time::sleep;
use tokio::{select, spawn};
use tracing::{error, info};

pub struct ConnectionMgr {
    pub(super) transport_id: AtomicU64,
    pub(super) by_serial: DashMap<String, Arc<Connection>>,
    pub(super) by_id: DashMap<u64, Arc<Connection>>,
    pub(super) devices_changed: Notify,
    pub(super) privkey: RsaPrivateKey,
    pub(super) forwardings: Arc<ForwardingMgr>,
}

impl ConnectionMgr {
    pub fn new(privkey: RsaPrivateKey, forwardings: Arc<ForwardingMgr>) -> Self {
        Self {
            transport_id: AtomicU64::new(1),
            by_serial: DashMap::new(),
            by_id: DashMap::new(),
            devices_changed: Notify::new(),
            privkey,
            forwardings,
        }
    }

    pub fn get(&self, serial: &str) -> Option<Ref<String, Arc<Connection>>> {
        self.by_serial.get(serial).filter(|c| !c.is_closed())
    }

    pub fn iter(&self) -> impl Iterator<Item = RefMulti<String, Arc<Connection>>> {
        self.by_serial.iter().filter(|c| !c.is_closed())
    }

    pub fn on_devices_changed(&self) -> Notified {
        self.devices_changed.notified()
    }

    pub(super) fn new_connection(
        &self, serial: String, backend: ConnectionBackend,
    ) -> Arc<Connection> {
        let connection = Arc::new(Connection {
            id: self.transport_id.fetch_add(1, Ordering::SeqCst),
            serial: serial.to_string(),
            backend,
            reverse_forwards: Default::default(),
        });

        info!(serial = connection.serial, id = connection.id, "created connection");

        self.by_serial.insert(connection.serial.clone(), connection.clone());
        self.by_id.insert(connection.id, connection.clone());

        self.devices_changed.notify_waiters();

        connection
    }

    fn remove_closed_connections(&self) {
        let mut changed = false;
        self.by_serial.retain(|serial, conn| {
            if conn.is_closed() {
                info!(id = conn.id, serial, "connection is closed");
                changed = true;
                false
            } else {
                true
            }
        });
        self.by_id.retain(|_, conn| !conn.is_closed());

        if changed {
            self.devices_changed.notify_waiters();
        }
    }

    pub async fn run(self: &Arc<Self>) -> Result<()> {
        let mut usb_watcher = config()
            .usb
            .enabled()
            .then(nusb::watch_devices)
            .transpose()
            .context("unable to watch usb devices")?;

        let this = self.clone();
        spawn(async move {
            loop {
                if config().usb.enabled() {
                    if let Err(err) = this.refresh_usb().await {
                        error!(?err, "refresh usb connections");
                    }
                }

                this.remove_closed_connections();

                select! {
                    Some(_) = usb_watcher.as_mut().unwrap().next(), if usb_watcher.is_some() => {},
                    _ = sleep(Duration::from_secs(5)) => {}
                }
            }
        });

        Ok(())
    }
}
