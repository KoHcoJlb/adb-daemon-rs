use crate::connection::ConnectionTrait;
use crate::error::ErrorKind;
use crate::message::{AdbMessage, AdbMessageHeader};
use crate::{Error, Result};
use derive_more::Display;
use nusb::transfer::{Direction, EndpointType, Queue, RequestBuffer, TransferError};
use nusb::{Device, DeviceInfo};
use std::mem;
use std::task::{ready, Context, Poll};
use tracing::{debug, info, instrument, trace, Level};

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
    in_queue: Queue<RequestBuffer>,
    out_queue: Queue<Vec<u8>>,
    packet_size: usize,

    in_header: Option<AdbMessageHeader>,
}

impl UsbConnection {
    #[cfg(not(windows))]
    fn open_device(device: &DeviceInfo) -> Result<(Device, u8)> {
        let iface = device
            .interfaces()
            .find(|i| {
                i.class() == ADB_CLASS
                    && i.subclass() == ADB_SUBCLASS
                    && i.protocol() == ADB_PROTOCOL
            })
            .ok_or(UsbError::Unsupported)?;
        Ok((device.open()?, iface.interface_number()))
    }

    #[cfg(windows)]
    fn open_device(device: &DeviceInfo) -> Result<(Device, u8)> {
        if device.vendor_id() != 0x18D1 || device.product_id() != 0x4EE7 {
            Err(UsbError::Unsupported)?
        }

        let device = device.open()?;
        let iface = device
            .active_configuration()
            .map_err(|_| Error::from((UsbError::Other, Error::msg("active configuration error"))))?
            .interface_alt_settings()
            .find(|i| {
                i.class() == ADB_CLASS
                    && i.subclass() == ADB_SUBCLASS
                    && i.protocol() == ADB_PROTOCOL
            })
            .map(|i| i.interface_number())
            .ok_or(UsbError::Unsupported)?;
        Ok((device, iface))
    }

    pub fn new(device: &DeviceInfo) -> Result<UsbConnection> {
        let (device, iface) = Self::open_device(device)?;

        let iface = device.claim_interface(iface).map_err(|e| (UsbError::Claim, e))?;

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

            in_header: None,
        })
    }
}

impl ConnectionTrait for UsbConnection {
    fn poll_read_message(&mut self, cx: &mut Context) -> Poll<Result<AdbMessage>> {
        if self.in_queue.pending() == 0 {
            self.in_queue.submit(RequestBuffer::new(AdbMessageHeader::LENGTH));
        }

        loop {
            let buf = ready!(self.in_queue.poll_next(cx))
                .into_result()
                .map_err(|e| UsbError::Transfer(e))?;
            trace!(buf = buf.len(), header = self.in_header.is_some());

            if let Some(header) = self.in_header.take() {
                return Poll::Ready(Ok(AdbMessage { header, payload: buf }));
            } else {
                let header = AdbMessageHeader::read(&buf[..])?;
                trace!(?header, "received header");

                if header.data_length > 0 {
                    self.in_queue.submit(RequestBuffer::new(header.data_length as usize));
                }
                self.in_queue.submit(RequestBuffer::reuse(buf, AdbMessageHeader::LENGTH));

                if header.data_length == 0 {
                    return Poll::Ready(Ok(AdbMessage { header, payload: vec![] }));
                }
                self.in_header = Some(header);
            }
        }
    }

    fn write_message(&mut self, mut msg: AdbMessage) -> Result<()> {
        assert_eq!(self.out_queue.pending(), 0, "must flush before write_message");
        trace!(?msg, "write message");

        self.out_queue.submit(msg.header.to_vec());
        if !msg.payload.is_empty() {
            self.out_queue.submit(mem::take(&mut msg.payload));

            if msg.header.data_length % self.packet_size as u32 == 0 {
                self.out_queue.submit(Vec::new());
            }
        }

        Ok(())
    }

    #[instrument(skip_all, fields(len = self.out_queue.pending()), ret(level = Level::TRACE))]
    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<()>> {
        while self.out_queue.pending() > 0 {
            ready!(self.out_queue.poll_next(cx)).into_result().map_err(UsbError::Transfer)?;
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for UsbConnection {
    fn drop(&mut self) {
        info!("dropped usb connection");
    }
}
