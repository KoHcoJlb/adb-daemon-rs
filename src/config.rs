use camino::Utf8PathBuf;
use eyre::Result;
use eyre::{Context, OptionExt};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::{env, fs, io};

pub fn config_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_eyre("no home dir").map(|d| d.join(".android"))
}

#[derive(Debug, Default, Deserialize)]
pub struct UsbConfig {
    enabled: Option<bool>,
    whitelist: Option<HashSet<String>>,
}

impl UsbConfig {
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn is_whitelisted(&self, serial: &str) -> bool {
        self.whitelist.as_ref().is_none_or(|l| l.contains(serial))
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    smart_socket_port: Option<u16>,
    #[serde(default)]
    pub usb: UsbConfig,
    pub transport_log: Option<Utf8PathBuf>,
}

impl Config {
    pub fn smart_socket_port(&self) -> u16 {
        self.smart_socket_port.unwrap_or(5037)
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn load_config() -> Result<()> {
    let path = env::var("ADB_DAEMON_CONFIG")
        .map(PathBuf::from)
        .or_else(|_| <Result<_>>::Ok(config_dir()?.join("adb-daemon.toml")))?;

    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::default(),
        Err(err) => Err(err).context("parse config file")?,
    };
    CONFIG.set(toml::from_str(&data).context("parse config file")?).unwrap();
    Ok(())
}

pub fn config() -> &'static Config {
    CONFIG.get().unwrap()
}
