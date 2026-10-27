use portable_pty::CommandBuilder;
use std::{
    ffi::OsString,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    os::unix::ffi::OsStringExt,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tui_frame::{
    Cancellation,
    frame::{EventKind, Frame},
    terminal::{Effect, Options, Terminal},
};

fn pty() -> (OwnedFd, OwnedFd) {
    let (mut m, mut s) = (-1, -1);
    let size = libc::winsize {
        ws_col: 40,
        ws_row: 10,
        ws_xpixel: 400,
        ws_ypixel: 200,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        },
        0
    );
    unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) }
}

#[test]
fn real_child_final_output_exit_metadata_and_terminal_restoration() {
    let (master, slave) = pty();
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let outer = std::thread::spawn(move || {
        let mut terminal = Terminal::new(Options::new(40, 10)).unwrap();
        let mut bytes = [0; 16384];
        while !stop.load(Ordering::Acquire) {
            let mut fd = libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut fd, 1, 10);
            };
            if fd.revents & libc::POLLIN != 0 {
                let n = unsafe {
                    libc::read(master.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len())
                };
                if n > 0 {
                    terminal.write(&bytes[..n as usize]).unwrap();
                    for effect in terminal.take_effects() {
                        if let Effect::Reply(b) = effect {
                            unsafe {
                                libc::write(master.as_raw_fd(), b.as_ptr().cast(), b.len());
                            }
                        }
                    }
                }
            }
        }
        (master, terminal.snapshot().unwrap())
    });
    let mut command = CommandBuilder::new("/bin/sh");
    command.env_clear();
    command.env("RAW", OsString::from_vec(vec![0xff, 0xfe]));
    command.env("SHELL", "/bin/sh");
    command.arg("-c");
    command.arg("printf 'hello-final'; exit 7");
    let frame = Frame::new(command, ());
    frame.terminal(slave.as_fd(), slave.as_fd()).unwrap();
    frame
        .header(1, |context| {
            context
                .view
                .set_string(0, 0, "Header", ratatui::style::Style::default());
        })
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let received = events.clone();
    frame
        .observe(move |e| received.lock().unwrap().push(e))
        .unwrap();
    let result = frame.run(&Cancellation::new()).unwrap();
    assert_eq!(result.process_state.unwrap().exit_code(), 7);
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".tmp")
        .join(format!("missing-interpreter-{}", std::process::id()));
    std::fs::write(&script, b"#!/does-not-exist/tui-frame-interpreter\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let missing = Frame::new(CommandBuilder::new(&script), ());
    missing.terminal(slave.as_fd(), slave.as_fd()).unwrap();
    let error = missing.run(&Cancellation::new()).unwrap_err();
    let tui_frame::Error::Pty(error) = error.error else {
        panic!("PTY failure lost its native error: {error:?}");
    };
    let mut cause: &(dyn std::error::Error + 'static) = error.as_ref();
    while cause.downcast_ref::<std::io::Error>().is_none() {
        cause = cause.source().expect("native spawn error chain");
    }
    assert_eq!(
        cause.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    std::fs::remove_file(script).unwrap();
    done.store(true, Ordering::Release);
    let (_master, _screen) = outer.join().unwrap();
    let events = events.lock().unwrap();
    let child = &events
        .iter()
        .find(|event| event.kind == EventKind::Started)
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap()
        .child;
    assert!(
        child
            .environment
            .contains(&OsString::from_vec(b"RAW=\xff\xfe".to_vec()))
    );
    assert!(child.environment.contains(&OsString::from("SHELL=/bin/sh")));
    assert_eq!(child.environment.len(), 4);
    assert_eq!(child.directory, std::env::current_dir().unwrap());
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::Started)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::Exited)
            .count(),
        1
    );
    let bytes = events
        .iter()
        .filter(|e| e.kind == EventKind::ChildOutput)
        .flat_map(|e| e.bytes.clone())
        .collect::<Vec<_>>();
    assert!(bytes.windows(11).any(|v| v == b"hello-final"));
    assert!(
        events
            .iter()
            .find(|e| e.kind == EventKind::Started)
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .child
            .pid
            > 0
    );
    let mut settings: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut settings) },
        0
    );
    assert_ne!(settings.c_lflag & libc::ICANON, 0);
    assert!(frame.run(&Cancellation::new()).is_err());
}

#[test]
fn cancellation_before_start_is_single_use_and_does_not_acquire_terminal() {
    let mut command = CommandBuilder::new("/bin/sh");
    command.arg("-c");
    command.arg("sleep 10");
    let frame = Frame::new(command, ());
    let cancellation = Cancellation::new();
    cancellation.cancel();
    assert!(matches!(
        frame.run(&cancellation).unwrap_err().error,
        tui_frame::Error::Cancelled
    ));
    assert!(matches!(
        frame.run(&Cancellation::new()).unwrap_err().error,
        tui_frame::Error::AlreadyRun
    ));
}
