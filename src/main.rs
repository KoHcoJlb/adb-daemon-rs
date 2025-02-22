mod config;
mod connection;
mod daemon;
mod forward;
mod log;
mod smart_socket;
mod util;

use crate::config::load_config;
use crate::daemon::AdbDaemon;
use eyre::{Result, WrapErr};
use std::fmt::{Debug, Formatter};
use tracing::info;

struct EyreHandler;

impl eyre::EyreHandler for EyreHandler {
    fn debug(
        &self, error: &(dyn std::error::Error + 'static), f: &mut Formatter<'_>,
    ) -> std::fmt::Result {
        Debug::fmt(error, f)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    eyre::set_hook(Box::new(|_| Box::new(EyreHandler))).unwrap();

    load_config().context("load config")?;

    let _guard = log::init();

    info!("Hello world");

    let daemon = AdbDaemon::new().await?;
    daemon.connections.run().await?;
    daemon.serve_smartsocket().await?;

    Ok(())
}
