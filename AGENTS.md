# Repository guidance

## Build and verification

- The workspace contains the `adb-daemon-rs` binary (Rust 2024) and `adb-transport` library (Rust 2021). Root Cargo commands select only the binary by default; use `--workspace` to include library test targets.
- Run `cargo check --workspace --all-targets --locked`, `cargo clippy --workspace --all-targets --locked`, and `cargo fmt --all -- --check` from the root. Formatting is configured in `rustfmt.toml`.
- There are currently no automated tests or fixtures. Use `cargo test --workspace --locked` when adding tests; append a test-name filter for focused execution. Passing Cargo checks does not exercise USB/device behavior.
- Prefer workspace verification for transport changes: the root manifest enables `nusb`'s `tokio` feature, while `packages/transport/Cargo.toml` does not.
- The root `Cargo.lock` governs workspace builds; the separately tracked `packages/transport/Cargo.lock` is not the workspace lockfile.
- `justfile` only lists recipes and optionally imports ignored `local.just`; local build/deploy recipes are machine-specific. `flake.nix` exposes `overlays.default`, with no standalone package or development-shell output.

## Execution and configuration

- `cargo run --locked -- server` starts the foreground server; omitting `server` does the same. `--daemon` only closes standard I/O; the Unix shim handles forking separately.
- Configuration comes from `ADB_DAEMON_CONFIG`, otherwise `~/.android/adb-daemon.toml`. The ignored repository-local `adb-daemon.toml` is not auto-loaded; use `ADB_DAEMON_CONFIG="$PWD/adb-daemon.toml" cargo run --locked -- server` to select it. A missing config file silently uses defaults, even with an explicit path.
- Defaults are `listen_address = "0.0.0.0:5037"` and USB enabled. Before binding, `src/daemon.rs` probes loopback on the configured port and sends `host:kill` to an existing server that does not identify as adb-daemon-rs. Use a separate port for isolated runtime checks.
- Startup requires an existing PKCS#8 PEM RSA private key, defaulting to `~/.android/adbkey`; `private_key` overrides the path. The daemon does not generate keys, and disabling USB does not remove this requirement.
- `[usb]` supports `enabled`, `include`, and `exclude` serial filters; exclusions win. `RUST_LOG` controls stderr logging; `transport_log` enables a separate trace-level file sink grouped by device serial.

## Code map

### Host daemon (`src/`)

- `main.rs` loads configuration, invokes the platform shim, parses CLI arguments, and binds or adopts the listener. `daemon.rs` handles existing-server detection, creates the Tokio runtime, loads the private key, and wires `ConnectionMgr`, `ForwardingMgr`, and the TCP accept loop.
- `config.rs` owns TOML deserialization, defaults, serial filtering, and the process-wide `OnceLock`. `log.rs` owns stderr filtering and rolling per-device file writers; file routing uses the `serial` field on tracing events/spans.
- `connection/manager.rs` owns live-device indexes by serial and ID, add/remove broadcasts, USB hotplug watching, and periodic refresh/cleanup. `connection/usb.rs` applies discovery policy, tries existing-key authentication then public-key offering, registers authenticated devices, and dispatches device-initiated sockets to forwarding.
- `connection/types.rs` is the daemon/library adapter: backend/socket wrappers and `Connection`/`WeakConnection` lifetimes. Weak handles retain identity, banner, tracing span, and reverse mappings; accessing the live backend requires `upgrade()`.
- `smart_socket/mod.rs` owns each client TCP session, `OKAY`/`FAIL` responses, and disconnect-aware waiting. `smart_socket/devices.rs` parses transport/serial selectors, resolves devices, and formats device lists. `smart_socket/services.rs` dispatches host services and bridges device-service streams. The four-hex-digit length-prefixed string helpers live in `util.rs`.
- `forward.rs` parses forward/reverse commands, manages host TCP listeners, and connects accepted device sockets to allowed reverse destinations. Reverse mappings live on each `WeakConnection`; reverse requests are recorded here and also sent to the device by `smart_socket/services.rs`.
- `sys/unix.rs` contains the adb shim, fork/exec, inherited-listener handling, and stdio redirection. `sys/windows.rs` supplies the platform equivalents, with a no-op adb shim.

### Device transport (`packages/transport/src/`)

- `lib.rs` defines the public API. This crate owns the device wire protocol and multiplexed streams; host service dispatch, device-selection policy, and TCP forwarding stay in the binary.
- `connection/mod.rs` defines the message-level `Connection` trait and backend enum (currently USB only). `connection/usb.rs` claims ADB interfaces, selects bulk endpoints, and reads/writes USB packets. This is the low-level I/O layer; discovery and authentication retry policy are in the daemon's `src/connection/usb.rs`.
- `transport.rs` owns `CNXN`/`AUTH` handshakes, advertised features, payload/window constants, the shared reader/message dispatcher, and opening/accepting logical sockets. `AuthTransport` becomes `Transport` after authentication; `PendingSocket` represents a device-initiated open awaiting acceptance.
- `socket.rs` implements each logical stream's Tokio `AsyncRead`/`AsyncWrite`, buffering, delayed-ACK flow control, and close behavior. `message.rs` defines wire commands, headers, and payloads.
- `auth.rs` encodes Android-format RSA public keys; handshake/signing logic is in `transport.rs`. `banner.rs` parses device type, properties, and features from `CNXN`; `error.rs` defines transport errors.

### Paths to trace

- Client service: TCP accept in `daemon.rs` → `SmartSocket::run` → selector parsing → `handle_service` → `ConnectionBackend::open_socket` → transport `OPEN`/stream I/O. Device-service payloads are relayed rather than interpreted by the daemon.
- Reverse connection: transport reader receives device `OPEN` → `Transport::accept_socket` in daemon `connection/usb.rs` → `ForwardingMgr::handle_socket` validates the reverse mapping and bridges to host loopback TCP.

## Protocol constraints

- `transport.rs` rejects devices whose banner lacks `delayed_ack`; `socket.rs` relies on its byte-credit acknowledgements. Compatibility changes must account for both files.
- Preserve the explicit half-close and TCP drain in `src/smart_socket/services.rs`: replacing it with a simple bidirectional copy can lose buffered output or reset the client connection.
- Preserve the periodic USB read wakeup in `packages/transport/src/connection/usb.rs`; its comment documents a Raspberry Pi 4 stall workaround. Flushing in `packages/transport/src/transport.rs` deliberately collects multiple task wakers so all waiting socket tasks can resume.
