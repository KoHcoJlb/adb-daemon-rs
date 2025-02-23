use crate::connection::ConnectionTrait;
use crate::error::ErrorKind;
use crate::message::{AdbMessage, AdbMessageHeader};
use crate::{Error, Result};
use derive_more::Display;
use nusb::transfer::{Direction, EndpointType, Queue, RequestBuffer, TransferError};
use nusb::{DeviceInfo, InterfaceInfo};
use std::future::poll_fn;
use std::mem;
use std::task::{ready, Poll};
use tokio::sync::Mutex;
use tracing::{debug, info, trace};

const ADB_CLASS: u8 = 0xff;
const ADB_SUBCLASS: u8 = 0x42;
const ADB_PROTOCOL: u8 = 0x1;

#[derive(Debug, Display, Clone)]
pub enum UsbError {
    Unsupported,
    Claim,
    Transfer(TransferError),
    Other,
}

impl From<UsbError> for Error {
    fn from(value: UsbError) -> Self {
        ErrorKind::Usb(value).into()
    }
}

impl From<nusb::Error> for Error {
    fn from(value: nusb::Error) -> Self {
        Error::new(ErrorKind::Usb(UsbError::Other), Box::new(value))
    }
}

pub struct UsbConnection {
    in_queue: Mutex<Queue<RequestBuffer>>,
    out_queue: Mutex<Queue<Vec<u8>>>,
    packet_size: usize,
}

fn find_interface(device: &DeviceInfo) -> Option<&InterfaceInfo> {
    device.interfaces().find(|i| {
        i.class() == ADB_CLASS && i.subclass() == ADB_SUBCLASS && i.protocol() == ADB_PROTOCOL
    })
}

impl UsbConnection {
    pub fn is_supported(device: &DeviceInfo) -> bool {
        find_interface(device).is_some()
    }

    pub fn new(device: &DeviceInfo) -> Result<UsbConnection> {
        let Some(iface) = find_interface(device) else { Err(UsbError::Unsupported)? };

        let device = device.open()?;

        let iface =
            device.claim_interface(iface.interface_number()).map_err(|e| (UsbError::Claim, e))?;

        let iface_desc = iface.descriptors().next().ok_or((UsbError::Other, "no interface"))?;

        let in_ep = iface_desc
            .endpoints()
            .find(|e| e.transfer_type() == EndpointType::Bulk && e.direction() == Direction::In)
            .ok_or((UsbError::Unsupported, "no in endpoint"))?;
        let out_ep = iface_desc
            .endpoints()
            .find(|e| e.transfer_type() == EndpointType::Bulk && e.direction() == Direction::Out)
            .ok_or((UsbError::Unsupported, "no out endpoint"))?;

        if in_ep.max_packet_size() != out_ep.max_packet_size() {
            Err((UsbError::Other, "in packet size != out packet size"))?;
        }

        let packet_size = in_ep.max_packet_size();
        debug!(packet_size);

        Ok(Self {
            in_queue: iface.bulk_in_queue(in_ep.address()).into(),
            out_queue: iface.bulk_out_queue(out_ep.address()).into(),
            packet_size,
        })
    }
}

impl ConnectionTrait for UsbConnection {
    async fn read_message(&self) -> Result<AdbMessage> {
        trace!("read message");

        let mut queue = self.in_queue.lock().await;

        // trace!("locked queue");

        if queue.pending() == 0 {
            queue.submit(RequestBuffer::new(AdbMessageHeader::LENGTH));
        }

        let buf = queue
            .next_complete()
            .await
            .into_result()
            .map_err(|e| (UsbError::Transfer(e), "read header"))?;
        let header = AdbMessageHeader::read(&buf[..])?;

        trace!(?header, "read header");

        let payload = if header.data_length > 0 {
            queue.submit(RequestBuffer::new(header.data_length as usize));

            queue
                .next_complete()
                .await
                .into_result()
                .map_err(|e| (UsbError::Transfer(e), "read payload"))?
        } else {
            Vec::new()
        };

        queue.submit(RequestBuffer::reuse(buf, AdbMessageHeader::LENGTH));

        Ok(AdbMessage { header, payload })
    }

    async fn write_message(&self, mut msg: AdbMessage) -> Result<()> {
        trace!(?msg, "write message");

        let mut queue = self.out_queue.lock().await;

        queue.submit(msg.header.to_vec());
        if !msg.payload.is_empty() {
            queue.submit(mem::take(&mut msg.payload));

            if msg.header.data_length % self.packet_size as u32 == 0 {
                queue.submit(Vec::new());
            }
        }

        poll_fn(|cx| {
            while queue.pending() > 0 {
                ready!(queue.poll_next(cx)).into_result().map_err(UsbError::Transfer)?;
            }

            Poll::Ready(<Result<_>>::Ok(()))
        })
        .await?;

        trace!(?msg, "written");

        Ok(())
    }
}

impl Drop for UsbConnection {
    fn drop(&mut self) {
        info!("dropped usb connection");
    }
}
