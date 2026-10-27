use portable_pty::CommandBuilder;
use std::{
    io::{self, BufRead, Read, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    os::unix::process::CommandExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tui_frame::{
    Cancellation, Error,
    frame::{Event, EventKind, Frame},
    input::KeyCode,
    routing::Capture,
    terminal::{Effect, Options, Terminal},
};

struct Outer {
    slave: OwnedFd,
    stop: Arc<AtomicBool>,
    writer: Arc<Mutex<OwnedFd>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Outer {
    fn new() -> Self {
        let (mut master, mut slave) = (-1, -1);
        let size = libc::winsize {
            ws_col: 40,
            ws_row: 12,
            ws_xpixel: 360,
            ws_ypixel: 216,
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
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let writer = Arc::new(Mutex::new(master));
        let read = writer.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let mut terminal = Terminal::new(Options::new(40, 12)).unwrap();
            let mut bytes = [0_u8; 16384];
            while !stopped.load(Ordering::Acquire) {
                let fd = read.lock().unwrap().as_raw_fd();
                let mut poll = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, 10) } <= 0 || poll.revents & libc::POLLIN == 0
                {
                    continue;
                }
                let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
                if count <= 0 {
                    break;
                }
                terminal.write(&bytes[..count as usize]).unwrap();
                for effect in terminal.take_effects() {
                    if let Effect::Reply(bytes) = effect {
                        write_fd(fd, &bytes);
                    }
                }
            }
        });
        Self {
            slave,
            stop,
            writer,
            worker: Some(worker),
        }
    }
    fn settings(&self) -> libc::termios {
        let mut settings = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(self.slave.as_raw_fd(), &mut settings) },
            0
        );
        settings
    }
}
impl Drop for Outer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

struct Deadline {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Deadline {
    fn new(cancellation: &Cancellation, timeout: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let cancel = cancellation.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + timeout;
            while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if !stopped.load(Ordering::Acquire) {
                cancel.cancel();
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Deadline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn duplicate(fd: i32) -> OwnedFd {
    let copied = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    assert!(
        copied >= 0,
        "duplicate descriptor: {}",
        io::Error::last_os_error()
    );
    unsafe { OwnedFd::from_raw_fd(copied) }
}
fn write_bounded(fd: i32, mut bytes: &[u8]) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    while !bytes.is_empty() {
        if Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "native input writer stalled",
            ));
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 50) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            continue;
        }
        if ready == 0 {
            continue;
        }
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written > 0 {
            bytes = &bytes[written as usize..];
        } else if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        } else {
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn paste() -> Vec<u8> {
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend(std::iter::repeat_n(b'x', 3 << 20));
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

fn has_error(error: &Error, check: fn(&Error) -> bool) -> bool {
    check(error)
        || match error {
            Error::Combined(group) => group.errors.iter().any(|error| has_error(error, check)),
            Error::Shared(error) => has_error(error, check),
            _ => false,
        }
}

struct DetachedHolder(Arc<Mutex<Option<i32>>>);
impl Drop for DetachedHolder {
    fn drop(&mut self) {
        if let Some(pid) = *self.0.lock().unwrap() {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}
fn write_fd(fd: i32, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        assert!(count > 0, "write: {}", io::Error::last_os_error());
        bytes = &bytes[count as usize..];
    }
}
fn command(mode: &str) -> CommandBuilder {
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.env_clear();
    command.env("TUI_FRAME_SESSION_FIXTURE", mode);
    command.args([
        "--ignored",
        "--exact",
        "native_session_fixture",
        "--nocapture",
    ]);
    command
}
fn output(events: &[Event]) -> Vec<u8> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::ChildOutput)
        .flat_map(|event| event.bytes.iter().copied())
        .collect()
}

#[test]
fn capture_and_callback_invalidation_route_only_unconsumed_input() {
    let outer = Outer::new();
    let before = outer.settings();
    let frame = Frame::new(command("input"), 0_u32);
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    let draws = Arc::new(Mutex::new(Vec::new()));
    let drawn = draws.clone();
    frame
        .header(1, move |context| {
            drawn.lock().unwrap().push(context.data);
        })
        .unwrap();
    let invalidate = frame.controller();
    frame
        .capture(move |key| {
            if key.event.code == KeyCode::Char('q') {
                invalidate
                    .invalidate(tui_frame::frame::Edge::Header, 7)
                    .unwrap();
                Capture::Consume
            } else {
                Capture::Pass
            }
        })
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let cancel = Cancellation::new();
    let fallback = cancel.clone();
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    let watchdog = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !finished.load(Ordering::Acquire) {
            fallback.cancel();
        }
    });
    let writer = outer.writer.clone();
    let mut sent = false;
    frame
        .observe(move |event| {
            let mut events = observed.lock().unwrap();
            events.push(event);
            if !sent
                && output(&events)
                    .windows(11)
                    .any(|bytes| bytes == b"INPUT_READY")
            {
                sent = true;
                write_fd(writer.lock().unwrap().as_raw_fd(), b"qpass\r");
            }
        })
        .unwrap();
    let result = frame.run(&cancel);
    done.store(true, Ordering::Release);
    watchdog.join().unwrap();
    let result = result.unwrap();
    assert!(result.process_state.unwrap().success());
    let events = events.lock().unwrap();
    assert!(
        output(&events)
            .windows(13)
            .any(|bytes| bytes == b"RECEIVED=pass")
    );
    assert!(events.iter().any(|event| event.kind == EventKind::Captured
        && event.bytes == b"q"
        && event.origin == "user-input"));
    assert!(
        !events
            .iter()
            .filter(|event| event.kind == EventKind::ChildInput)
            .flat_map(|event| &event.bytes)
            .any(|byte| *byte == b'q')
    );
    assert!(draws.lock().unwrap().contains(&7));
    let after = outer.settings();
    assert_eq!(
        (
            before.c_iflag,
            before.c_oflag,
            before.c_cflag,
            before.c_lflag,
            before.c_cc
        ),
        (
            after.c_iflag,
            after.c_oflag,
            after.c_cflag,
            after.c_lflag,
            after.c_cc
        )
    );
}

#[test]
fn live_cancellation_reaps_child_and_restores_the_outer_terminal() {
    let outer = Outer::new();
    let frame = Frame::new(command("wait"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    let cancellation = Cancellation::new();
    let cancel = cancellation.clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    frame
        .observe(move |event| {
            let started = event.kind == EventKind::Started;
            observed.lock().unwrap().push(event);
            if started {
                cancel.cancel();
            }
        })
        .unwrap();
    let started = Instant::now();
    let result = frame.run(&cancellation).unwrap_err();
    assert!(matches!(result.error, Error::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(result.result.process_state.is_some());
    assert!(result.result.cleanup_error.is_none());
    assert_ne!(outer.settings().c_lflag & libc::ICANON, 0);
    let events = events.lock().unwrap();
    let child = events
        .iter()
        .find(|event| event.kind == EventKind::Started)
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap()
        .child
        .pid;
    assert_eq!(unsafe { libc::kill(child, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == EventKind::Exited)
            .count(),
        1
    );
}

#[test]
fn child_protocol_replies_use_the_single_pty_writer_before_final_exit() {
    let outer = Outer::new();
    let frame = Frame::new(command("protocol"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    frame
        .observe(move |event| observed.lock().unwrap().push(event))
        .unwrap();
    let result = frame.run(&Cancellation::new()).unwrap();
    assert!(result.process_state.unwrap().success());
    let events = events.lock().unwrap();
    let reply = events
        .iter()
        .find(|event| event.kind == EventKind::ChildInput && event.bytes == b"\x1b[2;4R")
        .unwrap();
    assert_eq!(reply.origin, "terminal-reply");
    let protocol = events
        .iter()
        .find(|event| matches!(&event.effect, Some(Effect::Reply(bytes)) if bytes == b"\x1b[2;4R"))
        .unwrap();
    assert!(protocol.sequence < reply.sequence);
    assert!(output(&events).windows(8).any(|bytes| bytes == b"REPLY_OK"));
    assert!(
        events
            .windows(2)
            .all(|events| events[0].sequence < events[1].sequence)
    );
}

#[test]
fn observer_panic_cancels_child_without_escaping_or_losing_exit_status() {
    let outer = Outer::new();
    let frame = Frame::new(command("wait"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    frame
        .observe(|event| {
            if event.kind == EventKind::Started {
                panic!("root observer failed");
            }
        })
        .unwrap();
    let error = frame.run(&Cancellation::new()).unwrap_err();
    assert!(
        matches!(&error.error, Error::CallbackPanic(message) if message.starts_with("observer panicked: root observer failed\n") && message.contains("observation.rs")),
        "unexpected session error: {:?}",
        error.error
    );
    assert!(error.result.process_state.is_some());
    assert_ne!(outer.settings().c_lflag & libc::ICANON, 0);
}

#[test]
fn busy_child_applies_backpressure_and_receives_every_pasted_byte() {
    let outer = Outer::new();
    let frame = Frame::new(command("paste"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut ready = false;
    frame
        .observe(move |event| {
            let mut events = observed.lock().unwrap();
            events.push(event);
            if !ready
                && output(&events)
                    .windows(11)
                    .any(|bytes| bytes == b"PASTE_READY")
            {
                ready = true;
                sender.send(()).unwrap();
            }
        })
        .unwrap();
    let master = duplicate(outer.writer.lock().unwrap().as_raw_fd());
    let writer = thread::spawn(move || {
        receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        write_bounded(master.as_raw_fd(), &paste())
    });
    let cancellation = Cancellation::new();
    let _deadline = Deadline::new(&cancellation, Duration::from_secs(10));
    let result = frame.run(&cancellation);
    writer.join().unwrap().unwrap();
    assert!(result.unwrap().process_state.unwrap().success());
    let events = events.lock().unwrap();
    let delivered: Vec<u8> = events
        .iter()
        .filter(|event| event.kind == EventKind::ChildInput)
        .flat_map(|event| event.bytes.iter().copied())
        .collect();
    assert_eq!(delivered, paste());
    let envelope: Vec<u8> = events
        .iter()
        .filter(|event| event.kind == EventKind::ChildInput && event.origin == "paste-envelope")
        .flat_map(|event| event.bytes.iter().copied())
        .collect();
    assert_eq!(envelope, b"\x1b[200~\x1b[201~");
    let payload: Vec<u8> = events
        .iter()
        .filter(|event| event.kind == EventKind::ChildInput && event.origin == "user-input")
        .flat_map(|event| event.bytes.iter().copied())
        .collect();
    assert_eq!(payload, vec![b'x'; 3 << 20]);
    assert!(
        output(&events)
            .windows(14)
            .any(|bytes| bytes == b"PASTE_COMPLETE")
    );
    assert_ne!(outer.settings().c_lflag & libc::ICANON, 0);
}

#[test]
fn outer_hangup_terminates_the_child_and_returns_a_cleanup_outcome() {
    let mut outer = Outer::new();
    let frame = Frame::new(command("wait"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    let master = outer.writer.clone();
    let stop = outer.stop.clone();
    let mut reader = outer.worker.take();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    frame
        .observe(move |event| {
            let started = event.kind == EventKind::Started;
            observed.lock().unwrap().push(event);
            if started {
                stop.store(true, Ordering::Release);
                reader.take().unwrap().join().unwrap();
                let replacement: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
                drop(std::mem::replace(&mut *master.lock().unwrap(), replacement));
            }
        })
        .unwrap();
    let cancellation = Cancellation::new();
    let _deadline = Deadline::new(&cancellation, Duration::from_secs(4));
    let error = frame.run(&cancellation).unwrap_err();
    assert!(
        has_error(
            &error.error,
            |error| matches!(error, Error::Session(message) if message == "outer terminal disconnected" || message == "outer terminal input ended")
        ),
        "{:?}",
        error.error
    );
    assert!(error.result.process_state.is_some());
    assert!(
        error
            .result
            .cleanup_error
            .as_ref()
            .is_some_and(|error| has_error(
                error,
                |error| matches!(error, Error::Io(error) if error.raw_os_error() == Some(libc::EIO))
            ))
    );
    let events = events.lock().unwrap();
    let pid = events
        .iter()
        .find(|event| event.kind == EventKind::Started)
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap()
        .child
        .pid;
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn winch_applies_outer_cell_and_pixel_dimensions_to_the_native_child() {
    let outer = Outer::new();
    let frame = Frame::new(command("size"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    frame.header(1, |_| {}).unwrap();
    let master = duplicate(outer.writer.lock().unwrap().as_raw_fd());
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    frame
        .observe(move |event| {
            match event.kind {
                EventKind::Started => {
                    let size = libc::winsize {
                        ws_col: 50,
                        ws_row: 14,
                        ws_xpixel: 500,
                        ws_ypixel: 280,
                    };
                    assert_eq!(
                        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
                        0
                    );
                    assert_eq!(unsafe { libc::raise(libc::SIGWINCH) }, 0);
                }
                EventKind::Resized => write_fd(master.as_raw_fd(), b"size\r"),
                _ => (),
            }
            observed.lock().unwrap().push(event);
        })
        .unwrap();
    let cancellation = Cancellation::new();
    let _deadline = Deadline::new(&cancellation, Duration::from_secs(4));
    assert!(
        frame
            .run(&cancellation)
            .unwrap()
            .process_state
            .unwrap()
            .success()
    );
    let events = events.lock().unwrap();
    let snapshot = events
        .iter()
        .find(|event| event.kind == EventKind::Resized)
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap();
    let window = snapshot.child.pty.window.unwrap();
    assert_eq!(
        (
            snapshot.outer.width,
            snapshot.outer.height,
            snapshot.viewport.width,
            snapshot.viewport.height
        ),
        (50, 14, 50, 13)
    );
    assert_eq!(
        (
            window.ws_col,
            window.ws_row,
            window.ws_xpixel,
            window.ws_ypixel
        ),
        (50, 13, 500, 260)
    );
    assert!(
        output(&events)
            .windows(18)
            .any(|bytes| bytes == b"SIZE=50,13,500,260")
    );
}

#[test]
fn unsupported_child_column_change_returns_geometry_error_and_reaps_child() {
    let outer = Outer::new();
    let frame = Frame::new(command("deccolm"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    let cancellation = Cancellation::new();
    let _deadline = Deadline::new(&cancellation, Duration::from_secs(4));
    let error = frame.run(&cancellation).unwrap_err();
    assert!(
        has_error(&error.error, |error| matches!(
            error,
            Error::Geometry {
                owned: (40, 12),
                native: (132, 12)
            }
        )),
        "{:?}",
        error.error
    );
    assert!(error.result.process_state.is_some());
    assert_ne!(outer.settings().c_lflag & libc::ICANON, 0);
}

#[test]
fn detached_slave_holder_is_excluded_from_signals_and_bounds_output_draining() {
    let outer = Outer::new();
    let frame = Frame::new(command("detach"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    let holder = DetachedHolder(Arc::new(Mutex::new(None)));
    let holder_pid = holder.0.clone();
    let mut bytes = Vec::new();
    frame
        .observe(move |event| {
            if event.kind == EventKind::ChildOutput {
                bytes.extend_from_slice(&event.bytes);
                if let Some(pid) = String::from_utf8_lossy(&bytes)
                    .split_inclusive('\n')
                    .filter(|line| line.ends_with('\n'))
                    .find_map(|line| {
                        line.strip_prefix("HOLDER=")
                            .and_then(|pid| pid.trim().parse().ok())
                    })
                {
                    *holder_pid.lock().unwrap() = Some(pid);
                }
            }
        })
        .unwrap();
    let cancellation = Cancellation::new();
    let _deadline = Deadline::new(&cancellation, Duration::from_secs(5));
    let start = Instant::now();
    let error = frame.run(&cancellation).unwrap_err();
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert!(start.elapsed() < Duration::from_secs(4));
    assert!(
        error
            .error
            .to_string()
            .contains("child output did not close before drain deadline"),
        "{:?}",
        error.error
    );
    assert!(error.result.drain_error.is_some());
    assert!(error.result.process_state.unwrap().success());
    let pid = holder
        .0
        .lock()
        .unwrap()
        .expect("native detached holder PID was observed");
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "foreign session holder was signalled"
    );
    assert_eq!(tui_frame::process::read(pid).session_id.value, Some(pid));
    assert_ne!(outer.settings().c_lflag & libc::ICANON, 0);
}

#[test]
fn observer_controller_resizes_native_pty_without_retaining_the_frame() {
    let outer = Outer::new();
    let frame = Frame::new(command("size"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    frame.header(1, |_| {}).unwrap();
    let controller = frame.controller();
    let observed_controller = controller.clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let writer = outer.writer.clone();
    frame
        .observe(move |event| {
            match event.kind {
                EventKind::Started => observed_controller.set_border(true).unwrap(),
                EventKind::Resized => write_fd(writer.lock().unwrap().as_raw_fd(), b"size\r"),
                _ => (),
            }
            observed.lock().unwrap().push(event);
        })
        .unwrap();
    let result = frame.run(&Cancellation::new()).unwrap();
    assert!(result.process_state.unwrap().success());
    let events = events.lock().unwrap();
    let resized = events
        .iter()
        .find(|event| event.kind == EventKind::Resized)
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap();
    assert_eq!(
        (
            resized.viewport.x,
            resized.viewport.y,
            resized.viewport.width,
            resized.viewport.height
        ),
        (1, 2, 38, 9)
    );
    let window = resized.child.pty.window.unwrap();
    assert_eq!(
        (
            window.ws_col,
            window.ws_row,
            window.ws_xpixel,
            window.ws_ypixel
        ),
        (38, 9, 342, 162)
    );
    assert!(
        output(&events)
            .windows(14)
            .any(|bytes| bytes == b"SIZE=38,9,342,")
    );
    assert!(matches!(
        controller.set_border(false),
        Err(Error::SessionClosed)
    ));
    drop(events);
    drop(frame);
    assert!(matches!(
        controller.set_border(false),
        Err(Error::SessionClosed)
    ));
}

#[test]
fn unclosed_child_render_hold_is_released_before_exit() {
    let outer = Outer::new();
    let frame = Frame::new(command("hold"), ());
    frame
        .terminal(outer.slave.as_fd(), outer.slave.as_fd())
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    frame
        .observe(move |event| observed.lock().unwrap().push(event))
        .unwrap();
    let result = frame.run(&Cancellation::new()).unwrap();
    assert!(result.process_state.unwrap().success());
    let events = events.lock().unwrap();
    let held = events
        .iter()
        .filter_map(|event| event.snapshot.as_ref())
        .find(|snapshot| snapshot.terminal.input.held)
        .unwrap();
    assert_eq!(held.terminal.cells[0].text, "o");
    assert!(
        events
            .iter()
            .any(|event| matches!(event.effect, Some(Effect::RenderHold(false))))
    );
    let exited = events
        .iter()
        .find(|event| event.kind == EventKind::Exited)
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap();
    assert!(!exited.terminal.input.held);
    assert_eq!(exited.terminal.cells[0].text, "n");
}

#[test]
#[ignore = "real PTY child invoked by the behavioral session tests"]
#[allow(
    clippy::zombie_processes,
    reason = "The detach fixture intentionally exits before its child; the parent behavioral test owns and kills that detached holder"
)]
fn native_session_fixture() {
    match std::env::var("TUI_FRAME_SESSION_FIXTURE").unwrap().as_str() {
        "input" => {
            println!("INPUT_READY");
            io::stdout().flush().unwrap();
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line).unwrap();
            println!("RECEIVED={}", line.trim_end());
            assert_eq!(line.trim_end(), "pass");
        }
        "wait" => loop {
            thread::park_timeout(Duration::from_secs(60));
        },
        "protocol" => {
            let mut settings: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(0, &mut settings) }, 0);
            unsafe {
                libc::cfmakeraw(&mut settings);
            }
            assert_eq!(unsafe { libc::tcsetattr(0, libc::TCSANOW, &settings) }, 0);
            print!("\x1b[2;4H\x1b[6n");
            io::stdout().flush().unwrap();
            let mut response = [0_u8; 6];
            io::stdin().read_exact(&mut response).unwrap();
            assert_eq!(response, *b"\x1b[2;4R");
            print!("REPLY_OK");
            io::stdout().flush().unwrap();
        }
        "hold" => {
            print!("\x1b[2J\x1b[Hold\x1b[?2026h\rnew");
            io::stdout().flush().unwrap();
            thread::sleep(Duration::from_millis(600));
        }
        "size" => {
            let mut poll = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 2000) },
                1,
                "native size fixture received no input"
            );
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line).unwrap();
            assert_eq!(line.trim_end(), "size");
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) }, 0);
            println!(
                "SIZE={},{},{},{}",
                size.ws_col, size.ws_row, size.ws_xpixel, size.ws_ypixel
            );
        }
        "paste" => {
            let mut settings: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(0, &mut settings) }, 0);
            unsafe {
                libc::cfmakeraw(&mut settings);
            }
            assert_eq!(unsafe { libc::tcsetattr(0, libc::TCSANOW, &settings) }, 0);
            print!("\x1b[?2004hPASTE_READY");
            io::stdout().flush().unwrap();
            thread::sleep(Duration::from_millis(300));
            let mut bytes = vec![0_u8; paste().len()];
            io::stdin().read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, paste());
            println!("PASTE_COMPLETE");
        }
        "deccolm" => {
            print!("\x1b[?40h\x1b[?3h");
            io::stdout().flush().unwrap();
            thread::sleep(Duration::from_secs(6));
        }
        "detach" => {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--ignored",
                    "--exact",
                    "native_session_fixture",
                    "--nocapture",
                ])
                .env_clear()
                .env("TUI_FRAME_SESSION_FIXTURE", "detached-holder");
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            // The new session intentionally owns the inherited slave beyond leader exit.
            let holder = command.spawn().unwrap();
            println!("HOLDER={}", holder.id());
            io::stdout().flush().unwrap();
            std::process::exit(0);
        }
        "detached-holder" => thread::sleep(Duration::from_secs(7)),
        mode => panic!("unknown fixture {mode}"),
    }
}
