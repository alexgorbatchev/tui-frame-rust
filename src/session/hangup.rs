//! Darwin needs an edge-triggered read filter to observe hangup while input is paused.
use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

#[derive(Default)]
pub(super) struct HangupWatch {
    #[cfg(target_os = "macos")]
    queue: Option<OwnedFd>,
    #[cfg(target_os = "macos")]
    watching: bool,
}
impl HangupWatch {
    #[cfg(target_os = "macos")]
    pub(super) fn watch(&mut self, fd: i32, on: bool) -> io::Result<bool> {
        if on == self.watching {
            return Ok(false);
        }
        if self.queue.is_none() {
            let raw = crate::transport::cvt(unsafe { libc::kqueue() })?;
            let queue = unsafe { OwnedFd::from_raw_fd(raw) };
            crate::transport::cvt(unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) })?;
            self.queue = Some(queue);
        }
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = fd as libc::uintptr_t;
        change.filter = libc::EVFILT_READ;
        change.flags = if on {
            libc::EV_ADD | libc::EV_CLEAR
        } else {
            libc::EV_DELETE
        };
        loop {
            let result = unsafe {
                libc::kevent(
                    self.queue.as_ref().unwrap().as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if result >= 0 {
                self.watching = on;
                return Ok(false);
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::ENOENT) {
                self.watching = false;
                return Ok(on);
            }
            return Err(error);
        }
    }
    #[cfg(not(target_os = "macos"))]
    pub(super) fn watch(&mut self, _fd: i32, _on: bool) -> io::Result<bool> {
        Ok(false)
    }
    pub(super) fn poll_fd(&self) -> libc::pollfd {
        #[cfg(target_os = "macos")]
        if self.watching {
            return libc::pollfd {
                fd: self.queue.as_ref().unwrap().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
        }
        libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        }
    }
    #[cfg(target_os = "macos")]
    pub(super) fn hung_up(&self) -> io::Result<bool> {
        let Some(queue) = &self.queue else {
            return Ok(false);
        };
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let result = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        };
        if result >= 0 {
            return Ok(result > 0 && event.flags & libc::EV_EOF != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(error)
        }
    }
    #[cfg(not(target_os = "macos"))]
    pub(super) fn hung_up(&self) -> io::Result<bool> {
        Ok(false)
    }
}
