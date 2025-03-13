use crate::connection::types::{PendingSocket, WeakConnection};
use crate::smart_socket::{BUF_SIZE, SmartSocket, Status};
use derive_more::Display;
use eyre::{OptionExt, Result, WrapErr, bail, ensure};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::Write;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::spawn;
use tokio::task::JoinHandle;
use tracing::{Instrument, debug, error, info, info_span, trace, warn};

#[derive(Debug, Display, Eq, PartialEq, Hash, Clone)]
pub enum Proto {
    #[display("tcp:{_0}")]
    Tcp(u16),
    #[display("{_0}")]
    Other(String),
}

impl Proto {
    fn parse(s: &str) -> Result<Self> {
        Ok(if let Some(port) = s.strip_prefix("tcp:") {
            Self::Tcp(port.parse::<u16>().context("parse port")?)
        } else {
            Self::Other(s.to_string())
        })
    }
}

enum ForwardCommand {
    Add { left: Proto, right: Proto, norebind: bool },
    Remove(Proto),
    RemoveAll,
    List,
}

impl ForwardCommand {
    fn parse(service: &str) -> Result<Option<Self>> {
        Ok(if let Some(mut service) = service.strip_prefix("forward:") {
            let norebind = if let Some(s) = service.strip_prefix("norebind:") {
                service = s;
                true
            } else {
                false
            };

            let (left, right) = service.split_once(";").ok_or_eyre("invalid")?;

            Some(Self::Add {
                left: Proto::parse(left).context("parse local")?,
                right: Proto::parse(right).context("parse remote")?,
                norebind,
            })
        } else if let Some(service) = service.strip_prefix("killforward:") {
            Some(Self::Remove(Proto::parse(service)?))
        } else if service == "killforward-all" {
            Some(Self::RemoveAll)
        } else if service == "list-forward" {
            Some(Self::List)
        } else {
            None
        })
    }
}

struct Forward {
    conn: Arc<WeakConnection>,
    remote: Proto,
}

struct ForwardTask {
    target: Arc<RwLock<Forward>>,
    task: JoinHandle<()>,
}

impl Drop for ForwardTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
pub struct Reverse {
    local: Proto,
}

pub type ReverseForwards = RwLock<HashMap<Proto, Reverse>>;

pub struct ForwardingMgr {
    listeners: tokio::sync::RwLock<HashMap<Proto, ForwardTask>>,
}

async fn forward_task(listener: TcpListener, forward: Arc<RwLock<Forward>>) {
    loop {
        let mut local_socket = match listener.accept().await {
            Ok((s, _)) => s,
            Err(err) => {
                error!(?err, "accept failed");
                return;
            }
        };
        debug!("accepted socket");

        let (conn, remote) = {
            let f = forward.read();
            (if let Ok(conn) = f.conn.upgrade() { conn } else { return }, f.remote.clone())
        };

        let mut remote_socket = match conn.backend.open_socket(&remote.to_string()).await {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "open remote failed");
                continue;
            }
        };

        spawn(async move {
            let _ = tokio::io::copy_bidirectional_with_sizes(
                &mut local_socket,
                &mut remote_socket,
                BUF_SIZE,
                BUF_SIZE,
            )
            .await;
        });
    }
}

impl ForwardingMgr {
    pub fn new() -> Self {
        Self { listeners: Default::default() }
    }

    async fn clean_forwards(&self) {
        self.listeners
            .write()
            .await
            .retain(|_, f| !f.target.read().conn.is_closed() && !f.task.is_finished());
    }

    pub async fn handle_forward(&self, socket: &mut SmartSocket, service: &str) -> Result<bool> {
        let Some(cmd) = ForwardCommand::parse(service)? else {
            return Ok(false);
        };

        match cmd {
            ForwardCommand::Add { left: local, right: remote, norebind } => {
                self.clean_forwards().await;

                let local @ Proto::Tcp(port) = local else {
                    bail!("unsupported protocol: {local}")
                };

                let conn = socket.pick_connection()?;

                match self.listeners.write().await.entry(local.clone()) {
                    Entry::Occupied(entry) => {
                        ensure!(!norebind, "already bound");

                        info!(%local, %remote, "changed forward");
                        *entry.get().target.write() = Forward { conn, remote }
                    }
                    Entry::Vacant(entry) => {
                        let listener = TcpListener::bind(SocketAddr::new(
                            IpAddr::from_str("0.0.0.0").unwrap(),
                            port,
                        ))
                        .await
                        .context("bind")?;

                        info!(%local, %remote, "added forward");
                        let span = info_span!(parent: None, "forward", %local, %remote, serial = conn.serial);

                        let forward = Arc::new(RwLock::new(Forward { conn, remote }));
                        entry.insert(ForwardTask {
                            task: spawn(forward_task(listener, forward.clone()).instrument(span)),
                            target: forward,
                        });
                    }
                };
            }
            ForwardCommand::Remove(proto) => {
                self.clean_forwards().await;
                info!(?proto, "killforward");
                self.listeners.write().await.remove(&proto);
            }
            ForwardCommand::RemoveAll => {
                self.listeners.write().await.retain(|_, _| false);
            }
            ForwardCommand::List => {
                self.clean_forwards().await;

                let mut s = String::new();
                for (local, forward) in self.listeners.read().await.iter() {
                    let remote = forward.target.read();
                    let conn = remote.conn.upgrade();
                    let serial = conn.as_ref().map(|c| &c.serial[..]).unwrap_or("disconnected");
                    writeln!(s, "{serial} {} {}", local, remote.remote)?;
                }
                socket.respond_data(Status::Okay, s).await?;
                return Ok(true);
            }
        }

        socket.respond(Status::Okay).await?;
        socket.respond(Status::Okay).await?;
        Ok(true)
    }

    pub fn handle_reverse(&self, conn: &Arc<WeakConnection>, service: &str) -> Result<()> {
        let Some(service) = service.strip_prefix("reverse:") else {
            return Ok(());
        };
        let cmd = ForwardCommand::parse(service)?.ok_or_eyre("malformed reverse service")?;

        let mut forwards = conn.reverse_forwards.write();

        trace!(?forwards);

        match cmd {
            ForwardCommand::Add { left: remote, right: local, norebind } => {
                ensure!(!norebind || !forwards.contains_key(&remote), "already bound, would fail");

                debug!(%local, %remote, "added reverse forward");
                forwards.insert(remote, Reverse { local });
            }
            ForwardCommand::Remove(proto) => {
                if let Some(f) = forwards.remove(&proto) {
                    debug!(local = %f.local, remote = %proto, "removed reverse forward");
                }
            }
            ForwardCommand::RemoveAll => forwards.clear(),
            _ => {}
        }

        Ok(())
    }

    pub async fn handle_socket(&self, conn: &Arc<WeakConnection>, socket: PendingSocket) {
        let service = socket.service().into_owned();

        let mut socket = Some(socket);
        if let Err(err) = async {
            let proto @ Proto::Tcp(port) = Proto::parse(&service).context("parse proto")? else {
                bail!("unsupported protocol");
            };

            if !conn.reverse_forwards.read().values().any(|f| f.local == proto) {
                bail!("denied");
            }

            let mut local_socket =
                TcpStream::connect(SocketAddr::new(IpAddr::from_str("127.0.0.1").unwrap(), port))
                    .await
                    .context("connect")?;
            let mut socket = socket.take().unwrap().accept().await.context("accept")?;

            spawn(async move {
                let _ = tokio::io::copy_bidirectional_with_sizes(
                    &mut socket,
                    &mut local_socket,
                    BUF_SIZE,
                    BUF_SIZE,
                )
                .await;
            });

            <Result<_>>::Ok(())
        }
        .await
        {
            warn!(%service, ?err, "accept reverse socket");
            if let Some(socket) = socket {
                let _ = socket.close().await;
            }
        } else {
            debug!(%service, "accept reverse socket");
        }
    }
}
