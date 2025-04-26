mod config;
mod connection;
mod daemon;
mod forward;
mod log;
mod smart_socket;
mod sys;
mod util;

use crate::config::load_config;
use crate::sys::{RawSocket, adb_shim, close_stdio, socket_from_fd};
use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr, bail};
use std::fmt::{Debug, Formatter};

struct EyreHandler;

impl eyre::EyreHandler for EyreHandler {
    fn debug(
        &self, error: &(dyn std::error::Error + 'static), f: &mut Formatter<'_>,
    ) -> std::fmt::Result {
        Debug::fmt(error, f)
    }
}

#[derive(Debug, Args)]
struct ServerArgs {
    #[arg(long)]
    fd: Option<RawSocket>,
    #[arg(long)]
    daemon: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    Server(ServerArgs),
}

#[derive(Debug, Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}

fn main() -> Result<()> {
    eyre::set_hook(Box::new(|_| Box::new(EyreHandler))).unwrap();

    load_config().context("load config")?;

    adb_shim()?;

    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Command::Server(ServerArgs { fd: None, daemon: false }));

    let _guard = log::init(matches!(cmd, Command::Server(_)));
    // debug!(?cli);

    match cmd {
        Command::Server(args) => {
            let socket = if let Some(fd) = args.fd {
                socket_from_fd(fd)?
            } else {
                match daemon::bind_smartsocket().context("bind smartsocket")? {
                    Some(socket) => socket,
                    None => bail!("already running"),
                }
            };

            if args.daemon {
                close_stdio().context("close stdio")?;
            }

            daemon::start_daemon(socket)?;
        }
    }

    Ok(())
}
