use super::*;
use std::{
    io::Read,
    os::{
        fd::{AsFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    thread,
    time::{Duration, Instant},
};

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [-1; 2];
    cvt(unsafe { libc::pipe(fds.as_mut_ptr()) }).unwrap();
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

#[test]
fn finite_poll_timeout_and_readiness_preserve_native_eof_and_errors() {
    let (reader, writer) = pipe();
    let mut fds = [libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    let start = Instant::now();
    assert_eq!(poll(&mut fds, 15).unwrap(), 0);
    assert!(start.elapsed() >= Duration::from_millis(15));
    assert!(start.elapsed() < Duration::from_secs(2));
    write_all(writer.as_raw_fd(), b"native").unwrap();
    assert_eq!(poll(&mut fds, 500).unwrap(), 1);
    assert_ne!(fds[0].revents & libc::POLLIN, 0);
    let mut bytes = [0; 16];
    assert_eq!(read(reader.as_raw_fd(), &mut bytes).unwrap(), 6);
    assert_eq!(&bytes[..6], b"native");
    drop(writer);
    assert_eq!(poll(&mut fds, 500).unwrap(), 1);
    assert_ne!(fds[0].revents & libc::POLLHUP, 0);
    assert_eq!(read(reader.as_raw_fd(), &mut bytes).unwrap(), 0);
    assert_eq!(
        read(-1, &mut bytes).unwrap_err().raw_os_error(),
        Some(libc::EBADF)
    );
    assert_eq!(
        write(-1, b"bad").unwrap_err().raw_os_error(),
        Some(libc::EBADF)
    );
    assert_eq!(
        write_all(-1, b"bad").unwrap_err().raw_os_error(),
        Some(libc::EBADF)
    );
}

#[test]
fn nonblocking_restores_original_flags_explicitly_and_on_drop() {
    let (reader, _writer) = pipe();
    let flags = cvt(unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) }).unwrap();
    {
        let mut guard = Nonblocking::new(reader.as_fd()).unwrap();
        assert_eq!(
            read(reader.as_raw_fd(), &mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        guard.restore().unwrap();
        guard.restore().unwrap();
        assert_eq!(
            unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) },
            flags
        );
    }
    {
        let _guard = Nonblocking::new(reader.as_fd()).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
    }
    assert_eq!(
        unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) },
        flags
    );
}

#[test]
fn write_all_waits_for_real_backpressure_and_preserves_every_byte() {
    let (writer, mut reader) = UnixStream::pair().unwrap();
    let _guard = Nonblocking::new(writer.as_fd()).unwrap();
    let capacity: libc::c_int = 4096;
    cvt(unsafe {
        libc::setsockopt(
            writer.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&capacity as *const libc::c_int).cast(),
            size_of_val(&capacity) as libc::socklen_t,
        )
    })
    .unwrap();
    let prefix = [0x55; 4096];
    let mut prefix_len = 0;
    loop {
        match write(writer.as_raw_fd(), &prefix) {
            Ok(n) => prefix_len += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("filling socket: {e}"),
        }
    }
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let bytes = (0..262_144).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let total = prefix_len + bytes.len();
    let received = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        let mut result = vec![0; total];
        reader.read_exact(&mut result).unwrap();
        result
    });
    write_all(writer.as_raw_fd(), &bytes).unwrap();
    let received = received.join().unwrap();
    assert!(received[..prefix_len].iter().all(|b| *b == 0x55));
    assert_eq!(received[prefix_len..], bytes);
}

#[test]
fn native_pty_settings_and_window_round_trip_without_touching_process_terminal() {
    let (mut master, mut slave) = (-1, -1);
    let mut requested = libc::winsize {
        ws_row: 17,
        ws_col: 39,
        ws_xpixel: 390,
        ws_ypixel: 170,
    };
    cvt(unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut requested,
        )
    })
    .unwrap();
    let (_master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let observed = window(slave.as_raw_fd()).unwrap();
    assert_eq!(
        (
            observed.ws_row,
            observed.ws_col,
            observed.ws_xpixel,
            observed.ws_ypixel
        ),
        (17, 39, 390, 170)
    );
    let original = settings(slave.as_raw_fd()).unwrap();
    let mut raw = original;
    unsafe { libc::cfmakeraw(&mut raw) };
    set_settings(slave.as_raw_fd(), &raw).unwrap();
    let observed = settings(slave.as_raw_fd()).unwrap();
    assert_eq!(observed.c_lflag & (libc::ICANON | libc::ECHO), 0);
    set_settings(slave.as_raw_fd(), &original).unwrap();
    assert_eq!(
        settings(slave.as_raw_fd()).unwrap().c_lflag,
        original.c_lflag
    );
    assert_eq!(settings(-1).unwrap_err().raw_os_error(), Some(libc::EBADF));
    assert_eq!(
        set_settings(-1, &original).unwrap_err().raw_os_error(),
        Some(libc::EBADF)
    );
    assert_eq!(window(-1).unwrap_err().raw_os_error(), Some(libc::EBADF));
}
