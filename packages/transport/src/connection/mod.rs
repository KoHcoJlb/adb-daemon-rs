use crate::message::AdbMessage;
use crate::Result;
use enum_dispatch::enum_dispatch;
use std::future::Future;
use usb::UsbConnection;

pub mod usb;

#[enum_dispatch]
pub trait ConnectionTrait {
    fn read_message(&self) -> impl Future<Output = Result<AdbMessage>> + Send;

    fn write_message(&self, msg: AdbMessage) -> impl Future<Output = Result<()>> + Send;
}

#[enum_dispatch(ConnectionTrait)]
pub enum Connection {
    UsbConnection,
}
