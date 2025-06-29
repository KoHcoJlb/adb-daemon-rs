use crate::message::AdbMessage;
use crate::Result;
use enum_dispatch::enum_dispatch;
use std::task::{Context, Poll};
use usb::UsbConnection;

pub mod usb;

#[enum_dispatch]
pub trait ConnectionTrait {
    fn poll_read_message(&mut self, cx: &mut Context) -> Poll<Result<AdbMessage>>;

    fn write_message(&mut self, msg: AdbMessage) -> Result<()>;

    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<()>>;
}

#[enum_dispatch(ConnectionTrait)]
pub enum Connection {
    UsbConnection,
}

impl<C: ConnectionTrait> ConnectionTrait for &mut C {
    fn poll_read_message(&mut self, cx: &mut Context) -> Poll<Result<AdbMessage>> {
        (*self).poll_read_message(cx)
    }

    fn write_message(&mut self, msg: AdbMessage) -> Result<()> {
        (*self).write_message(msg)
    }

    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<()>> {
        (*self).poll_flush(cx)
    }
}
