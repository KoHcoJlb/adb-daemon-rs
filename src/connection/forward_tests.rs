use super::manager::ConnectionMgr;
use super::types::{ConnectionBackend, MaybeConnection};
use crate::config::init_test_config;
use crate::daemon::AdbDaemon;
use crate::forward::ForwardingMgr;
use crate::smart_socket::SmartSocket;
use crate::util::write_protocol_string;
use rsa::{BigUint, RsaPrivateKey};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::time::{sleep, timeout};

const SERIAL: &str = "forward-test-device";
const TIMEOUT: Duration = Duration::from_secs(5);

fn pstring(data: &str) -> String {
    format!("{:04x}{data}", data.len())
}

struct Fixture {
    daemon: Arc<AdbDaemon>,
    opened: UnboundedReceiver<(String, DuplexStream)>,
}

impl Fixture {
    fn new() -> Self {
        init_test_config();

        // A small deterministic RSA key suffices: this backend never authenticates.
        let key = RsaPrivateKey::from_components(
            BigUint::from(3233u32),
            BigUint::from(17u32),
            BigUint::from(2753u32),
            vec![BigUint::from(61u32), BigUint::from(53u32)],
        )
        .unwrap();
        let forwardings = Arc::new(ForwardingMgr::new());
        let connections = Arc::new(ConnectionMgr::new(key, forwardings.clone()));
        let (opened, receiver) = unbounded_channel();
        connections
            .new_connection(
                MaybeConnection::alloc(SERIAL),
                ConnectionBackend::Test {
                    banner: "device::features=delayed_ack".parse().unwrap(),
                    opened,
                },
            )
            .unwrap();

        Self { daemon: Arc::new(AdbDaemon { connections, forwardings }), opened: receiver }
    }

    async fn request(&self, service: &str) -> String {
        timeout(TIMEOUT, async {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
            let (server, _) = listener.accept().await.unwrap();
            let task = tokio::spawn(SmartSocket::run(self.daemon.clone(), server));

            write_protocol_string(&mut client, format!("host-serial:{SERIAL}:{service}"))
                .await
                .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            task.await.unwrap();
            response
        })
        .await
        .expect("smart-socket request timed out")
    }

    async fn allocate(&self, remote: &str, norebind: bool) -> u16 {
        let prefix = if norebind { "norebind:" } else { "" };
        let response = self.request(&format!("forward:{prefix}tcp:0;{remote}")).await;
        assert!(response.starts_with("OKAYOKAY"), "{response:?}");
        assert!(response.len() > 12, "missing allocated port: {response:?}");

        let port: u16 = response[12..].parse().unwrap();
        assert_ne!(port, 0);
        assert_eq!(response, format!("OKAYOKAY{}", pstring(&port.to_string())));
        port
    }

    async fn assert_list(&self, entries: &[(u16, &str)]) {
        let response = self.request("list-forward").await;
        assert!(response.starts_with("OKAY"), "{response:?}");
        assert!(response.len() >= 8, "{response:?}");
        assert_eq!(response, format!("OKAY{}", pstring(&response[8..])));

        let mut actual: Vec<_> = response[8..].lines().collect();
        let mut expected: Vec<_> =
            entries.iter().map(|(port, remote)| format!("{SERIAL} tcp:{port} {remote}")).collect();
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }

    async fn assert_forward(&mut self, port: u16, remote: &str) {
        timeout(TIMEOUT, async {
            let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
            let (service, mut device) = self.opened.recv().await.unwrap();
            assert_eq!(service, remote);

            client.write_all(b"host").await.unwrap();
            let mut buf = [0; 4];
            device.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"host");

            device.write_all(b"peer").await.unwrap();
            client.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"peer");
        })
        .await
        .expect("forwarded I/O timed out");
    }
}

async fn assert_removed(port: u16) {
    // Listener removal aborts its task; wait for cancellation to release the socket.
    timeout(TIMEOUT, async {
        loop {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                Ok(_) => break,
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                    sleep(Duration::from_millis(10)).await;
                }
                Err(err) => panic!("unexpected bind error: {err}"),
            }
        }
    })
    .await
    .expect("removed forward still owns its port");
}

async fn check_automatic_allocations(norebind: bool) {
    let mut fixture = Fixture::new();
    let remote_a = "localabstract:first";
    let remote_b = "localabstract:second";

    let first = fixture.allocate(remote_a, norebind).await;
    let second = fixture.allocate(remote_b, norebind).await;

    assert_ne!(first, second);
    fixture.assert_list(&[(first, remote_a), (second, remote_b)]).await;
    fixture.assert_forward(first, remote_a).await;
    fixture.assert_forward(second, remote_b).await;

    assert_eq!(fixture.request(&format!("killforward:tcp:{first}")).await, "OKAYOKAY");

    fixture.assert_list(&[(second, remote_b)]).await;
    assert_removed(first).await;
    fixture.assert_forward(second, remote_b).await;
}

#[tokio::test]
async fn automatic_ports_are_returned_listed_and_independently_removable() {
    check_automatic_allocations(false).await;
}

#[tokio::test]
async fn norebind_automatic_ports_allocate_independently() {
    check_automatic_allocations(true).await;
}

#[tokio::test]
async fn fixed_port_rebind_preserves_listener_and_norebind_preserves_target() {
    let mut fixture = Fixture::new();
    let original = "localabstract:original";
    let replacement = "localabstract:replacement";
    let port = fixture.allocate(original, false).await;

    let response = fixture.request(&format!("forward:tcp:{port};{replacement}")).await;

    assert_eq!(response, "OKAYOKAY");
    fixture.assert_list(&[(port, replacement)]).await;
    fixture.assert_forward(port, replacement).await;

    let response = fixture.request(&format!("forward:norebind:tcp:{port};{original}")).await;

    assert_eq!(response, format!("FAIL{}", pstring("already bound")));
    fixture.assert_list(&[(port, replacement)]).await;
    fixture.assert_forward(port, replacement).await;

    assert_eq!(fixture.request(&format!("killforward:tcp:{port}")).await, "OKAYOKAY");
    assert_removed(port).await;

    // Recreate using a fixed port to cover the initial bind path as well as rebinds.
    assert_eq!(
        fixture.request(&format!("forward:norebind:tcp:{port};{original}")).await,
        "OKAYOKAY"
    );
    fixture.assert_list(&[(port, original)]).await;
    fixture.assert_forward(port, original).await;

    assert_eq!(fixture.request("killforward-all").await, "OKAYOKAY");
    fixture.assert_list(&[]).await;
    assert_removed(port).await;
}
