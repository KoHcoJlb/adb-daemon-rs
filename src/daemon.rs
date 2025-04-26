use crate::config::{config, config_dir};
use crate::connection::manager::ConnectionMgr;
use crate::forward::ForwardingMgr;
use crate::smart_socket::SmartSocket;
use eyre::{Result, WrapErr, ensure};
use rsa::RsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::{io, net};
use tokio::net::TcpListener;
use tokio::{fs, runtime, spawn};
use tracing::{debug, trace};

fn check_adb_server() -> Result<bool> {
    trace!("check adb server");

    fn run_service(svc: &str) -> io::Result<bool> {
        let mut s = TcpStream::connect(("127.0.0.1", config().smart_socket_port()))?;

        s.write_all(format!("{:04x}{svc}", svc.len()).as_bytes())?;

        let mut status = [0; 4];
        s.read_exact(&mut status)?;

        s.read_exact(&mut [1])
            .or_else(|e| if e.kind() == io::ErrorKind::UnexpectedEof { Ok(()) } else { Err(e) })?;

        Ok(&status == b"OKAY")
    }

    Ok(match run_service("host:is-adb-daemon-rs") {
        Ok(true) => true,
        Ok(false) => {
            debug!("stopping adb");
            ensure!(run_service("host:kill").context("kill")?, "failed to kill adb");
            false
        }
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => false,
        Err(e) => Err(e)?,
    })
}

pub fn bind_smartsocket() -> Result<Option<net::TcpListener>> {
    loop {
        if check_adb_server()? {
            return Ok(None);
        } else {
            match net::TcpListener::bind(("0.0.0.0", config().smart_socket_port())) {
                Ok(s) => return Ok(Some(s)),
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => {}
                Err(e) => Err(e).context("bind smartsocket")?,
            };
        }
    }
}

pub fn start_daemon(socket: net::TcpListener) -> Result<()> {
    runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        socket.set_nonblocking(true)?;

        let daemon = AdbDaemon::new().await?;
        daemon.connections.run().await?;
        daemon.serve_smartsocket(socket.try_into()?).await?;
        Ok(())
    })
}

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

    pub async fn serve_smartsocket(self: Arc<Self>, smart_socket: TcpListener) -> Result<()> {
        loop {
            let (socket, _) = smart_socket.accept().await.context("accept connection")?;

            spawn(SmartSocket::run(self.clone(), socket));
        }
    }
}
