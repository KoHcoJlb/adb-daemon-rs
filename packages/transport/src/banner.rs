use crate::{Error, ErrorKind};
use derive_more::Display;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

// device::ro.product.name=raven;ro.product.model=Pixel 6 Pro;ro.product.device=raven;features=shell_v2,cmd,stat_v2,ls_v2,fixed_push_mkdir,apex,abb,fixed_push_symlink_timestamp,abb_exec,remount_shell,track_app,sendrecv_v2,sendrecv_v2_brotli,sendrecv_v2_lz4,sendrecv_v2_zstd,sendrecv_v2_dry_run_send,openscreen_mdns,delayed_ack

#[derive(Debug, Display, Clone, PartialEq, Eq)]
pub enum DeviceType {
    Device,
    Other(String),
}

#[derive(Debug, Clone)]
pub struct Banner {
    pub device_type: DeviceType,
    pub properties: HashMap<String, String>,
    pub features: HashSet<String>,
}

impl Banner {
    pub const PRODUCT_NAME: &'static str = "ro.product.name";
    pub const PRODUCT_MODEL: &'static str = "ro.product.model";
    pub const PRODUCT_DEVICE: &'static str = "ro.product.device";

    pub fn features_str(&self) -> String {
        let mut s = self.features.iter().fold(String::new(), |acc, f| acc + f + ",");
        s.pop();
        s
    }

    pub fn getprop(&self, key: &str) -> Option<&str> {
        Some(self.properties.get(key)?.as_str())
    }
}

impl FromStr for Banner {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (r#type, properties) = s.split_once("::").ok_or(ErrorKind::InvalidData)?;
        let mut properties = properties
            .split(';')
            .map(|prop| {
                prop.split_once('=')
                    .ok_or(ErrorKind::InvalidData)
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        let features = properties.remove("features").unwrap_or_default();

        let r#type = match r#type {
            "device" => DeviceType::Device,
            v => DeviceType::Other(v.to_string()),
        };

        Ok(Self {
            device_type: r#type,
            properties,
            features: features.split(",").map(|x| x.into()).collect::<HashSet<_>>(),
        })
    }
}
