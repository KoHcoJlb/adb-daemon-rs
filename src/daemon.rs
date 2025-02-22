use crate::config::{config, config_dir};
use crate::connection::manager::ConnectionMgr;
use crate::forward::ForwardingMgr;
use crate::smart_socket::SmartSocket;
use eyre::{Result, WrapErr};
use rsa::RsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::{fs, spawn};

pub struct AdbDaemon {
    pub connections: Arc<ConnectionMgr>,
    pub forwardings: Arc<ForwardingMgr>,
}

impl AdbDaemon {
    pub async fn new() -> Result<Arc<Self>> {
        let privkey =
            fs::read_to_string(config_dir()?.join("adbkey")).await.context("read private key")?;
        let privkey = RsaPrivateKey::from_pkcs8_pem(&privkey).context("decode private key")?;

        let forwardings = Arc::new(ForwardingMgr::new());
        Ok(Arc::new(AdbDaemon {
            connections: Arc::new(ConnectionMgr::new(privkey, forwardings.clone())),
            forwardings,
        }))
    }

    pub async fn serve_smartsocket(self: Arc<Self>) -> Result<()> {
        let smart_socket = TcpListener::bind(SocketAddr::new(
            IpAddr::from_str("0.0.0.0").unwrap(),
            config().smart_socket_port(),
        ))
        .await
        .context("bind socket")?;

        loop {
            let (socket, _) = smart_socket.accept().await.context("accept connection")?;

            spawn(SmartSocket::run(self.clone(), socket));
        }
    }
}
