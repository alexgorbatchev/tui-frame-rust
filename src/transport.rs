use std::{
    io,
    os::fd::{AsRawFd, BorrowedFd, RawFd},
};

pub(crate) fn cvt(value: libc::c_int) -> io::Result<libc::c_int> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

pub(crate) fn read(fd: RawFd, bytes: &mut [u8]) -> io::Result<usize> {
    let result = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

pub(crate) fn write(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    let result = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

pub(crate) fn write_all(fd: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match write(fd, bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let mut fds = [libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                }];
                poll(&mut fds, -1)?;
                if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn poll(fds: &mut [libc::pollfd], timeout: i32) -> io::Result<usize> {
    let deadline = (timeout >= 0)
        .then(|| std::time::Instant::now() + std::time::Duration::from_millis(timeout as u64));
    loop {
        let timeout = deadline
            .map(|due| {
                due.saturating_duration_since(std::time::Instant::now())
                    .as_millis()
                    .saturating_add(u128::from(timeout != 0))
                    .min(i32::MAX as u128) as i32
            })
            .unwrap_or(-1);
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if result >= 0 {
            return Ok(result as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub(crate) fn settings(fd: RawFd) -> io::Result<libc::termios> {
    let mut result = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::tcgetattr(fd, &mut result) })?;
    Ok(result)
}

pub(crate) fn set_settings(fd: RawFd, value: &libc::termios) -> io::Result<()> {
    cvt(unsafe { libc::tcsetattr(fd, libc::TCSANOW, value) })?;
    Ok(())
}

pub(crate) fn window(fd: RawFd) -> io::Result<libc::winsize> {
    let mut result = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut result) })?;
    Ok(result)
}

pub(crate) struct Nonblocking<'a> {
    fd: BorrowedFd<'a>,
    flags: Option<i32>,
}
impl<'a> Nonblocking<'a> {
    pub(crate) fn new(fd: BorrowedFd<'a>) -> io::Result<Self> {
        let flags = cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })?;
        cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
        Ok(Self {
            fd,
            flags: Some(flags),
        })
    }
    pub(crate) fn restore(&mut self) -> io::Result<()> {
        if let Some(flags) = self.flags {
            cvt(unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, flags) })?;
            self.flags = None;
        }
        Ok(())
    }
}
impl Drop for Nonblocking<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
#[path = "../tests/unit/transport.rs"]
mod tests;
