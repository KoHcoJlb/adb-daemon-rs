use crate::config::config;
use crate::connection::types::{Connection, ConnectionBackend, MaybeConnection, WeakConnection};
use crate::forward::ForwardingMgr;
use dashmap::DashMap;
use dashmap::mapref::multiple::RefMulti;
use dashmap::mapref::one::Ref;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use rsa::RsaPrivateKey;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::sleep;
use tokio::{select, spawn};
use tracing::{error, info};

#[derive(Debug, Clone)]
#[allow(unused)]
pub enum DeviceUpdate {
    Added(Arc<WeakConnection>),
    Removed(Arc<WeakConnection>),
}

pub struct ConnectionMgr {
    pub(super) by_serial: DashMap<String, Arc<Connection>>,
    pub(super) by_id: DashMap<u64, Arc<Connection>>,

    device_updates: broadcast::Sender<DeviceUpdate>,

    pub(super) privkey: RsaPrivateKey,
    pub(super) forwardings: Arc<ForwardingMgr>,
}

fn connection_filter(c: &Connection) -> bool {
    !c.is_closed()
}

impl ConnectionMgr {
    pub fn new(privkey: RsaPrivateKey, forwardings: Arc<ForwardingMgr>) -> Self {
        let (tx, _) = broadcast::channel(128);

        Self {
            by_serial: DashMap::new(),
            by_id: DashMap::new(),
            device_updates: tx,
            privkey,
            forwardings,
        }
    }

    pub fn get(&self, serial: &str) -> Option<Ref<'_, String, Arc<Connection>>> {
        self.by_serial.get(serial).filter(|c| connection_filter(c))
    }

    pub fn iter(&self) -> impl Iterator<Item = RefMulti<'_, String, Arc<Connection>>> {
        self.by_serial.iter().filter(|c| connection_filter(c))
    }

    pub fn device_updates(&self) -> broadcast::Receiver<DeviceUpdate> {
        self.device_updates.subscribe()
    }

    pub(super) fn new_connection(
        &self, conn: MaybeConnection, backend: ConnectionBackend,
    ) -> Result<Arc<Connection>> {
        let connection = conn.create(backend)?;

        info!(parent: &connection.span, "created connection");

        self.by_serial.insert(connection.serial.clone(), connection.clone());
        self.by_id.insert(connection.id, connection.clone());

        let _ = self.device_updates.send(DeviceUpdate::Added(connection.downgrade()));

        Ok(connection)
    }

    fn remove_closed_connections(&self) {
        let mut removed = vec![];
        self.by_serial.retain(|_, conn| {
            if conn.is_closed() {
                removed.push(conn.downgrade());
                false
            } else {
                true
            }
        });

        for conn in removed {
            let _span = conn.span.clone().entered();
            info!("connection is closed");
            self.by_id.remove(&conn.id);
            let _ = self.device_updates.send(DeviceUpdate::Removed(conn));
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
