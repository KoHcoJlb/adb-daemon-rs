use crate::message::AdbMessage;
use crate::Result;
use delegate::delegate;
use derive_more::From;
use std::task::{Context, Poll};
use usb::UsbConnection;

pub mod usb;

pub trait Connection {
    fn poll_read_message(&mut self, cx: &mut Context) -> Poll<Result<AdbMessage>>;

    fn write_message(&mut self, msg: AdbMessage) -> Result<()>;

    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<()>>;
}

#[derive(From)]
pub enum ConnectionEnum {
    UsbConnection(UsbConnection),
}

impl Connection for ConnectionEnum {
    delegate! {
        to match self {
            ConnectionEnum::UsbConnection(c) => c,
        } {
            fn poll_read_message(&mut self, cx: &mut Context) -> Poll<Result<AdbMessage>>;

            fn write_message(&mut self, msg: AdbMessage) -> Result<()>;

            fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<()>>;
        }
    }
}

impl<C: Connection> Connection for &mut C {
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
