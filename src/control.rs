use crate::transport;
use std::{
    io,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Default)]
pub struct Cancellation {
    inner: Arc<CancelState>,
}
#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    wakes: Mutex<Vec<Weak<Wake>>>,
}
impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        for wake in self
            .inner
            .wakes
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
        {
            wake.notify();
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }
    pub(crate) fn register(&self, wake: &Arc<Wake>) {
        let mut wakes = self.inner.wakes.lock().unwrap();
        wakes.retain(|w| w.strong_count() > 0);
        wakes.push(Arc::downgrade(wake));
    }
}

pub(crate) struct Wake {
    read: OwnedFd,
    write: Mutex<Option<OwnedFd>>,
    error: Mutex<Option<String>>,
}
impl Wake {
    pub(crate) fn new() -> io::Result<Arc<Self>> {
        let mut fds = [-1; 2];
        transport::cvt(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        for fd in [&read, &write] {
            let flags = transport::cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })?;
            transport::cvt(unsafe {
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
            })?;
            transport::cvt(unsafe {
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC)
            })?;
        }
        Ok(Arc::new(Self {
            read,
            write: Mutex::new(Some(write)),
            error: Mutex::new(None),
        }))
    }
    pub(crate) fn fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }
    pub(crate) fn notify(&self) {
        let mut writer = self.write.lock().unwrap();
        let Some(fd) = writer.as_ref() else {
            return;
        };
        loop {
            match transport::write(fd.as_raw_fd(), b"w") {
                Ok(_) => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    *self.error.lock().unwrap() = Some(e.to_string());
                    *writer = None;
                    return;
                }
            }
        }
    }
    pub(crate) fn drain(&self) -> io::Result<()> {
        let mut buf = [0; 256];
        loop {
            match transport::read(self.read.as_fd().as_raw_fd(), &mut buf) {
                Ok(0) => return Err(io::ErrorKind::BrokenPipe.into()),
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
    pub(crate) fn failure(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[path = "../tests/unit/control.rs"]
mod tests;
