use crate::error::ErrorKind;
use crate::Result;
use bytes::{Buf, BufMut};
#[allow(unused_imports)]
use derive_more::{Debug, TryFrom};
use std::cmp::min;
use std::fmt;
use std::fmt::Formatter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFrom)]
#[try_from(repr)]
#[repr(u32)]
pub enum AdbCommand {
    SYNC = 0x434e5953,
    CNXN = 0x4e584e43,
    OPEN = 0x4e45504f,
    OKAY = 0x59414b4f,
    CLSE = 0x45534c43,
    WRTE = 0x45545257,
    AUTH = 0x48545541,
    STLS = 0x534C5453,
}

#[derive(Debug)]
pub struct AdbMessageHeader {
    pub command: AdbCommand,
    pub arg0: u32,
    pub arg1: u32,
    pub data_length: u32,
    pub crc32: u32,
    pub magic: u32,
}

impl AdbMessageHeader {
    pub const LENGTH: usize = 24;

    pub fn read(mut buf: impl Buf) -> Result<Self> {
        if buf.remaining() < Self::LENGTH {
            Err((ErrorKind::InvalidData, "header too short"))?;
        }

        Ok(Self {
            command: AdbCommand::try_from(buf.get_u32_le())
                .map_err(|_| (ErrorKind::InvalidData, "unknown command"))?,
            arg0: buf.get_u32_le(),
            arg1: buf.get_u32_le(),
            data_length: buf.get_u32_le(),
            crc32: buf.get_u32_le(),
            magic: buf.get_u32_le(),
        })
    }

    pub fn write(&self, mut buf: impl BufMut) {
        buf.put_u32_le(self.command as u32);
        buf.put_u32_le(self.arg0);
        buf.put_u32_le(self.arg1);
        buf.put_u32_le(self.data_length);
        buf.put_u32_le(self.crc32);
        buf.put_u32_le(self.magic);
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.write(&mut buf);
        buf
    }
}

#[allow(dead_code)]
struct PayloadFmt<'a>(&'a Vec<u8>);

impl fmt::Debug for PayloadFmt<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let slice = bstr::BStr::new(&self.0[..min(100, self.0.len())]);
        write!(f, "{:?}", slice)?;
        if slice.len() < self.0.len() {
            f.write_str("...")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct AdbMessage {
    pub header: AdbMessageHeader,
    #[debug("{:?}", PayloadFmt(&self.payload))]
    pub payload: Vec<u8>,
}

impl AdbMessage {
    pub fn new(command: AdbCommand, arg0: u32, arg1: u32, payload: Vec<u8>) -> Self {
        Self {
            header: AdbMessageHeader {
                command,
                arg0,
                arg1,
                data_length: payload.len() as u32,
                crc32: 0,
                magic: command as u32 ^ 0xffffffff,
            },
            payload,
        }
    }
}
