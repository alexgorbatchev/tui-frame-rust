use std::{
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    time::Duration,
};
use tui_frame::{
    console::Console,
    terminal::{Effect, Options, Terminal},
};

fn pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    let size = libc::winsize {
        ws_col: 80,
        ws_row: 24,
        ws_xpixel: 720,
        ws_ypixel: 432,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        },
        0
    );
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

#[test]
fn real_pty_probes_restore_modes_termios_and_borrowed_descriptors() {
    let (master, slave) = pty();
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut original) },
        0
    );
    let mut console = Console::new(slave.as_fd(), slave.as_fd(), true).unwrap();
    assert!(Console::new(slave.as_fd(), slave.as_fd(), true).is_err());
    let worker = std::thread::spawn(move || {
        let mut outer = Terminal::new(Options::new(80, 24)).unwrap();
        let mut bytes = [0; 16384];
        let n = unsafe { libc::read(master.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        assert!(n > 0);
        outer.write(&bytes[..n as usize]).unwrap();
        let mut replies = b"typed".to_vec();
        for effect in outer.take_effects() {
            if let Effect::Reply(data) = effect {
                replies.extend(data);
            }
        }
        assert_eq!(
            unsafe { libc::write(master.as_raw_fd(), replies.as_ptr().cast(), replies.len()) },
            replies.len() as isize
        );
        std::thread::sleep(Duration::from_millis(100));
        master
    });
    let saved = console
        .probe(Duration::from_secs(1), || false, |_| Ok(()))
        .unwrap();
    assert_eq!(
        saved.iter().flat_map(|p| p.raw.clone()).collect::<Vec<_>>(),
        b"typed"
    );
    assert!(console.grapheme_width());
    console.enter().unwrap();
    let mut child = Terminal::new(Options::new(10, 3)).unwrap();
    child
        .write(b"\x1b[?1003;1006;1004h\x1b[>5u\x1b[>4;2m")
        .unwrap();
    console.sync_input(&child.input_state().unwrap()).unwrap();
    console.flush().unwrap();
    console.restore().unwrap();
    let _master = worker.join().unwrap();
    let mut after: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut after) }, 0);
    assert_eq!(
        (
            original.c_iflag,
            original.c_oflag,
            original.c_cflag,
            original.c_lflag,
            original.c_cc
        ),
        (
            after.c_iflag,
            after.c_oflag,
            after.c_cflag,
            after.c_lflag,
            after.c_cc
        )
    );
    assert!(Console::new(slave.as_fd(), slave.as_fd(), false).is_ok());
}

#[test]
fn startup_requires_switchable_inactive_alternate_screen_and_cancellation() {
    let (_master, slave) = pty();
    let mut console = Console::new(slave.as_fd(), slave.as_fd(), false).unwrap();
    assert!(
        console
            .probe(Duration::from_millis(1), || false, |_| Ok(()))
            .is_err()
    );
    assert!(
        console
            .probe(Duration::from_millis(50), || true, |_| Ok(()))
            .is_err()
    );
}
