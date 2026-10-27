use super::*;
use std::{fs::File, time::Instant};

fn ready(wake: &Wake, timeout: i32) -> i16 {
    let mut fds = [libc::pollfd {
        fd: wake.fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    transport::poll(&mut fds, timeout).unwrap();
    fds[0].revents
}

#[test]
fn cancellation_wakes_every_live_registration_and_is_shared_across_threads() {
    let cancel = Cancellation::new();
    let expired = Wake::new().unwrap();
    cancel.register(&expired);
    drop(expired);
    let wakes = [Wake::new().unwrap(), Wake::new().unwrap()];
    for wake in &wakes {
        cancel.register(wake);
    }
    assert_eq!(cancel.inner.wakes.lock().unwrap().len(), 2);
    assert!(!cancel.is_cancelled());
    let other = cancel.clone();
    std::thread::spawn(move || other.cancel()).join().unwrap();
    assert!(cancel.is_cancelled());
    for wake in &wakes {
        assert_ne!(ready(wake, 500) & libc::POLLIN, 0);
        wake.drain().unwrap();
        assert_eq!(ready(wake, 0), 0);
    }
    cancel.cancel();
    for wake in &wakes {
        assert_ne!(ready(wake, 500) & libc::POLLIN, 0);
    }
}

#[test]
fn full_nonblocking_wake_pipe_coalesces_notifications_without_losing_readiness() {
    let wake = Wake::new().unwrap();
    for fd in [
        wake.fd(),
        wake.write.lock().unwrap().as_ref().unwrap().as_raw_fd(),
    ] {
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
    let bytes = [1; 4096];
    let fd = wake.write.lock().unwrap().as_ref().unwrap().as_raw_fd();
    loop {
        match transport::write(fd, &bytes) {
            Ok(n) => assert!(n > 0),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("filling wake pipe: {e}"),
        }
    }
    let start = Instant::now();
    for _ in 0..100 {
        wake.notify();
    }
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    assert!(wake.failure().is_none());
    assert_ne!(ready(&wake, 0) & libc::POLLIN, 0);
    wake.drain().unwrap();
    assert_eq!(ready(&wake, 0), 0);
    wake.notify();
    assert_ne!(ready(&wake, 500) & libc::POLLIN, 0);
}

#[test]
fn native_write_failure_is_retained_and_disconnects_wake_reader() {
    let wake = Wake::new().unwrap();
    // A genuinely read-only descriptor makes write fail without violating OwnedFd ownership.
    *wake.write.lock().unwrap() = Some(File::open("/dev/null").unwrap().into());
    wake.notify();
    assert!(wake.failure().is_some());
    assert!(wake.write.lock().unwrap().is_none());
    assert_ne!(ready(&wake, 500) & libc::POLLHUP, 0);
    assert_eq!(wake.drain().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    let failure = wake.failure();
    wake.notify();
    assert_eq!(wake.failure(), failure);
}
