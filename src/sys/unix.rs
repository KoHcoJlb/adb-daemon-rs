use crate::daemon::bind_smartsocket;
use eyre::eyre;
use eyre::{OptionExt, Result, WrapErr};
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl, open};
use nix::sys::stat::Mode;
use nix::unistd::{ForkResult, close, dup2, execv, fork};
use std::borrow::Cow;
use std::env;
use std::ffi::{CStr, CString, OsStr};
use std::io::{stderr, stdin, stdout};
use std::iter::once;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd};
use which::which_all;

pub type RawSocket = std::os::fd::RawFd;

pub fn adb_shim() -> Result<()> {
    let exe = env::current_exe()?;
    if exe.file_name() == Some(OsStr::new("adb")) {
        let real_exe = exe.read_link().map_err(|_| eyre!("must be a link for adb shim to work"))?;

        if let Some(socket) = bind_smartsocket().context("bind smartsocket")? {
            fcntl(socket.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::empty()))
                .context("clear cloexec")?;

            match unsafe { fork().context("fork")? } {
                ForkResult::Parent { .. } => {}
                ForkResult::Child => {
                    let bin = CString::new(real_exe.into_os_string().into_encoded_bytes())?;
                    let port = CString::new(format!("{}", socket.as_raw_fd()))?;
                    let args = [bin.as_ref(), c"server", c"--daemon", c"--fd", &port];
                    execv(&bin, &args)?;
                }
            }
        }

        let bin = which_all("adb")
            .context("find adb")?
            .find(|p| p != &exe)
            .map(|p| CString::new(p.into_os_string().into_encoded_bytes()))
            .ok_or_eyre("adb not found")??;

        let args: Vec<Cow<CStr>> = once(Ok((&bin).into()))
            .chain(env::args().skip(1).map(|s| CString::new(s).map(|s| s.into())))
            .collect::<Result<_, _>>()?;
        execv(&bin, &args).context("exec adb")?;
    }

    Ok(())
}

pub fn socket_from_fd(fd: RawSocket) -> Result<TcpListener> {
    fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).context("set cloexec")?;
    unsafe { Ok(TcpListener::from_raw_fd(fd)) }
}

pub fn close_stdio() -> Result<()> {
    let null = open("/dev/null", OFlag::O_RDWR, Mode::empty()).context("open /dev/null")?;
    dup2(null, stdin().as_raw_fd()).context("dup2 stdin")?;
    dup2(null, stdout().as_raw_fd()).context("dup2 stdout")?;
    dup2(null, stderr().as_raw_fd()).context("dup2 stderr")?;
    close(null)?;
    Ok(())
}
