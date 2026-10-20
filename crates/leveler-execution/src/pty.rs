//! PTY descriptors owned by the execution substrate, never by a UI client.
use crate::command::PtySize;
use std::{
    io,
    os::fd::{AsRawFd, OwnedFd},
    sync::Arc,
};
use tokio::io::unix::AsyncFd;

pub struct PtyMaster(AsyncFd<OwnedFd>);
impl PtyMaster {
    pub fn new(fd: OwnedFd) -> io::Result<Arc<Self>> {
        use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
        fcntl(fd.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(io::Error::from)?;
        fcntl(fd.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).map_err(io::Error::from)?;
        Ok(Arc::new(Self(AsyncFd::new(fd)?)))
    }
    pub async fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.0.readable().await?;
            match ready
                .try_io(|fd| nix::unistd::read(fd.as_raw_fd(), bytes).map_err(io::Error::from))
            {
                Ok(result) => return result,
                Err(_) => continue,
            }
        }
    }
    pub async fn write(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let mut ready = self.0.writable().await?;
            if let Ok(result) =
                ready.try_io(|fd| nix::unistd::write(fd.get_ref(), bytes).map_err(io::Error::from))
            {
                let n = result?;
                if n == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                bytes = &bytes[n..];
            }
        }
        Ok(())
    }
    #[allow(unsafe_code)]
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        let winsize = nix::pty::Winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the live master descriptor is owned by self; the kernel reads
        // exactly one initialized winsize for TIOCSWINSZ during this call.
        if unsafe { nix::libc::ioctl(self.0.as_raw_fd(), nix::libc::TIOCSWINSZ, &winsize) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[allow(unsafe_code)]
pub fn attach(cmd: &mut tokio::process::Command, slave: OwnedFd) -> io::Result<()> {
    let slave: std::fs::File = slave.into();
    cmd.stdin(slave.try_clone()?)
        .stdout(slave.try_clone()?)
        .stderr(slave);
    // SAFETY: this hook performs only async-signal-safe OS calls after fork.
    // Stdin is the owned slave duplicated by Command before pre_exec; setsid
    // makes this child the group leader before acquiring its controlling tty.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(io::Error::from)?;
            if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Enumerate only processes still in the terminal's OS session. Job-control
/// shells create additional process groups inside this same session.
pub(crate) fn session_members(sid: i32) -> io::Result<Vec<nix::unistd::Pid>> {
    let pids = process_ids()?;
    Ok(pids
        .into_iter()
        .filter_map(|pid| {
            let pid = nix::unistd::Pid::from_raw(pid);
            (nix::unistd::getsid(Some(pid)).ok()?.as_raw() == sid).then_some(pid)
        })
        .collect())
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process_ids() -> io::Result<Vec<i32>> {
    // SAFETY: null/zero queries the required count; the subsequent buffer is
    // initialized, aligned for pid_t, and sized in bytes as libproc requires.
    let estimate = unsafe { nix::libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if estimate < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut capacity = estimate as usize + 1024;
    for _ in 0..3 {
        let mut pids = vec![0i32; capacity];
        // SAFETY: pids owns capacity initialized i32 slots for this synchronous call.
        let count = unsafe {
            nix::libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (capacity * size_of::<i32>()) as i32,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if (count as usize) < capacity {
            pids.truncate(count as usize);
            pids.retain(|pid| *pid > 0);
            return Ok(pids);
        }
        capacity *= 2;
    }
    Err(io::Error::other("process enumeration changed repeatedly"))
}
#[cfg(target_os = "linux")]
fn process_ids() -> io::Result<Vec<i32>> {
    std::fs::read_dir("/proc")?
        .filter_map(|entry| match entry {
            Ok(entry) => entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<i32>().ok())
                .filter(|pid| *pid > 0)
                .map(Ok),
            Err(error) => Some(Err(error)),
        })
        .collect()
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_ids() -> io::Result<Vec<i32>> {
    Err(io::Error::other("Unsupported: terminal session inspection"))
}
