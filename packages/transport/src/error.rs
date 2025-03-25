use crate::connection::usb::UsbError;
use derive_more::{Display, From};
use std::sync::Arc;

#[derive(Debug, Display, From)]
#[non_exhaustive]
pub enum ErrorKind {
    InvalidData,
    Closed,
    Usb(UsbError),
    Sign,
    AuthRejected,
    AlreadyAuthenticated,
    NotAuthenticated,
    InvalidKey,
    TransportError(Arc<Error>),
    WriteTimeout,
    DelayedAckNotAvailable,
    Other,
    Msg(String),
}

impl ErrorKind {
    pub(crate) fn msg(msg: impl AsRef<str>) -> Self {
        ErrorKind::Msg(msg.as_ref().into())
    }
}

#[derive(Debug, Display)]
#[display("{kind}")]
pub struct Error {
    pub kind: ErrorKind,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    pub(crate) fn new(
        kind: impl Into<ErrorKind>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self { kind: kind.into(), source: Some(source.into()) }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let ErrorKind::TransportError(error) = &self.kind {
            return Some(&**error);
        }
        self.source.as_ref().map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl From<ErrorKind> for Error {
    fn from(value: ErrorKind) -> Self {
        Self { kind: value, source: None }
    }
}

impl<K: Into<ErrorKind>, E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>> From<(K, E)>
    for Error
{
    fn from(value: (K, E)) -> Self {
        Self::new(value.0, value.1)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
