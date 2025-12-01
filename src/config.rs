use camino::Utf8PathBuf;
use eyre::Result;
use eyre::{Context, OptionExt};
use serde::Deserialize;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::OnceLock;
use std::{env, fs, io};

pub fn config_dir() -> Result<Utf8PathBuf> {
    Ok(Utf8PathBuf::try_from(dirs::home_dir().ok_or_eyre("no home dir")?)?.join(".android"))
}

#[derive(Debug, Default, Deserialize)]
pub struct UsbConfig {
    enabled: Option<bool>,
    #[serde(default)]
    include: HashSet<String>,
    #[serde(default)]
    exclude: HashSet<String>,
}

impl UsbConfig {
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn is_whitelisted(&self, serial: &str) -> bool {
        (self.include.is_empty() || self.include.contains(serial)) && !self.exclude.contains(serial)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    listen_address: Option<SocketAddr>,
    #[serde(default)]
    pub usb: UsbConfig,
    pub transport_log: Option<Utf8PathBuf>,
    private_key: Option<Utf8PathBuf>,
}

impl Config {
    pub fn listen_address(&self) -> SocketAddr {
        self.listen_address.unwrap_or((Ipv4Addr::new(0, 0, 0, 0), 5037).into())
    }

    pub fn private_key(&self) -> Result<Utf8PathBuf> {
        self.private_key.clone().map(Ok).unwrap_or_else(|| Ok(config_dir()?.join("adbkey")))
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn load_config() -> Result<()> {
    let path = env::var("ADB_DAEMON_CONFIG")
        .map(Utf8PathBuf::from)
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
