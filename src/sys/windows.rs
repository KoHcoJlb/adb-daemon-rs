use eyre::{Result, WrapErr};
use std::net::TcpListener;
use std::os::windows::io::FromRawSocket;
use windows::Win32::Storage::FileSystem::{
    CreateFileA, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_MODE,
    OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows::core::{Free, s};

pub type RawSocket = std::os::windows::io::RawSocket;

pub fn adb_shim() -> Result<()> {
    Ok(())
}

pub fn socket_from_fd(raw: RawSocket) -> Result<TcpListener> {
    unsafe { Ok(TcpListener::from_raw_socket(raw)) }
}

pub fn close_stdio() -> Result<()> {
    unsafe {
        let mut nul = CreateFileA(
            s!(r"\\.\NUL"),
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_MODE::default(),
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .context("open nul")?;

        SetStdHandle(STD_INPUT_HANDLE, nul).context("set stdin")?;
        SetStdHandle(STD_OUTPUT_HANDLE, nul).context("set stdout")?;
        SetStdHandle(STD_ERROR_HANDLE, nul).context("set stderr")?;

        nul.free();

        Ok(())
    }
}
