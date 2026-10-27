use crate::{
    Error, Result,
    console::Console,
    control::Cancellation,
    draw::{Compositor, Damage},
    frame::{
        ChildSnapshot, Event, EventKind, Frame, Geometry, PtySnapshot, RunError, RunResult,
        Snapshot,
    },
    input::{Event as InputEvent, Packet},
    observation::Dispatcher,
    process,
    routing::{Capture, HostProfile, Origin, Router},
    terminal::{Effect, State, Terminal},
    transport,
};
use portable_pty::{MasterPty, PtySize};
use signal_hook::iterator::Signals;
use std::{
    collections::VecDeque,
    io,
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};
mod hangup;

const QUEUE_LIMIT: usize = 1 << 20;
const READ_SIZE: usize = 64 << 10;
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);
const ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);
const HOLD_TIMEOUT: Duration = Duration::from_millis(250);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const CONFLICTS: &[&str] = &[
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "LC_TERMINAL",
    "KONSOLE_VERSION",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "GHOSTTY_RESOURCES_DIR",
    "GHOSTTY_BIN_DIR",
    "WEZTERM_EXECUTABLE",
    "WEZTERM_PANE",
    "ITERM_SESSION_ID",
    "TERM_SESSION_ID",
    "XTERM_VERSION",
    "MLTERM",
    "TERMINAL_NAME",
    "EAT_SHELL_INTEGRATION_DIR",
    "WARP_HONOR_PS1",
    "WARP_SESSION_ID",
    "WARP_TERMINAL_SESSION_UUID",
    "WARP_IS_LOCAL_SHELL_SESSION",
    "VSCODE_INJECTION",
    "TABBY_CONFIG_DIRECTORY",
    "VTE_VERSION",
    "GHOSTTY_SHELL_FEATURES",
    "WT_SESSION",
    "ITERM_PROFILE",
    "CURSOR_TRACE_ID",
    "TERM_PROGRAM_VERSION",
    "LC_TERMINAL_VERSION",
    "TERM_FEATURES",
    "COLUMNS",
    "LINES",
];

impl<T: Clone + Send + 'static> Frame<'_, T> {
    /// Execute once. Child nonzero status is an outcome; session failures retain it.
    pub fn run(&self, cancel: &Cancellation) -> std::result::Result<RunResult, RunError> {
        let mut result = RunResult::default();
        if let Err(error) = self.begin(cancel) {
            return Err(RunError { error, result });
        }
        cancel.register(&self.wake);
        let mut failure = None;
        let (input, output) = *self.terminal.lock().unwrap();
        let (inherit, timeout, observer, kinds) = {
            let mut c = self.config.lock().unwrap();
            (
                c.inherit,
                c.probe_timeout,
                c.observer.take(),
                c.observed.clone(),
            )
        };
        let mut dispatcher =
            observer.map(|handler| Dispatcher::new(kinds, handler, self.wake.clone()));
        let execute = (|| -> Result<()> {
            let mut signals = Signals::new([libc::SIGWINCH])?;
            let signal_handle = signals.handle();
            let resized = Arc::new(AtomicBool::new(false));
            let resized_signal = resized.clone();
            let wake = self.wake.clone();
            let signal_thread = thread::spawn(move || {
                for _ in signals.forever() {
                    resized_signal.store(true, Ordering::Release);
                    wake.notify();
                }
            });
            let signal_guard = SignalGuard {
                handle: signal_handle,
                thread: Some(signal_thread),
            };
            let mut console = Console::new(input, output, inherit)?;
            let run = (|| -> Result<()> {
                let window = console.window()?;
                let geometry = self.layout(window.ws_col, window.ws_row)?;
                let saved = console.probe(
                    timeout,
                    || {
                        cancel.is_cancelled()
                            || dispatcher.as_ref().is_some_and(|d| d.failure().is_some())
                    },
                    |bytes| {
                        let mut event = Event::new(EventKind::OuterInput);
                        event.bytes = bytes.to_vec();
                        event.origin = "terminal".into();
                        emit(dispatcher.as_ref(), event)
                    },
                )?;
                if console.cell_width == 0 {
                    console.cell_width = u32::from(window.ws_xpixel) / u32::from(window.ws_col);
                }
                if console.cell_height == 0 {
                    console.cell_height = u32::from(window.ws_ypixel) / u32::from(window.ws_row);
                }
                let terminal = Terminal::new(
                    console.child_options(geometry.child.width, geometry.child.height),
                )?;
                let (terminal, state) = terminal_state_initial(terminal)?;
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let mut command = self.config.lock().unwrap().command.clone();
                let directory = match command.get_cwd() {
                    Some(directory) => std::path::PathBuf::from(directory),
                    None => std::env::current_dir()?,
                };
                if !directory.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        "child working directory does not exist or is not a directory",
                    )
                    .into());
                }
                command.cwd(&directory);
                for name in CONFLICTS {
                    command.env_remove(name);
                }
                command.env("TERM", "xterm-256color");
                command.env("COLORTERM", "truecolor");
                let mut environment = std::collections::BTreeMap::from([(
                    std::ffi::OsString::from("SHELL"),
                    std::ffi::OsString::from(command.get_shell()),
                )]);
                environment.extend(
                    command
                        .iter_full_env()
                        .map(|(key, value)| (key.to_owned(), value.to_owned())),
                );
                let environment = environment
                    .into_iter()
                    .map(|(mut key, value)| {
                        key.push("=");
                        key.push(value);
                        key
                    })
                    .collect();
                let pair = portable_pty::native_pty_system()
                    .openpty(pty_size(&geometry, &console))
                    .map_err(|error| {
                        Error::Pty(error.reallocate_into_boxed_dyn_error_without_backtrace())
                    })?;
                let fd = pair
                    .master
                    .as_raw_fd()
                    .ok_or_else(|| Error::Session("native PTY provides no descriptor".into()))?;
                if inherit {
                    transport::set_settings(fd, console.original_settings())?;
                }
                let mut child = pair.slave.spawn_command(command.clone()).map_err(|error| {
                    Error::Pty(error.reallocate_into_boxed_dyn_error_without_backtrace())
                })?;
                drop(pair.slave);
                let pid = child
                    .process_id()
                    .ok_or_else(|| Error::Session("native PTY child provides no PID".into()))?
                    as i32;
                let (send, receive) = mpsc::channel();
                let wake = self.wake.clone();
                let waiter = thread::spawn(move || {
                    let _ = send.send(child.wait());
                    wake.notify();
                });
                let started_at = SystemTime::now();
                let os = process::read(pid);
                let child_snapshot = ChildSnapshot {
                    pid,
                    executable: command.get_argv()[0].clone(),
                    args: command.get_argv().clone(),
                    environment,
                    directory,
                    started_at,
                    process_state: None,
                    operating_system: os,
                    session_processes: Vec::new(),
                    session_error: None,
                    pty: pty_snapshot(fd),
                };
                let mut session = Session {
                    frame: self,
                    console: &mut console,
                    terminal,
                    dispatcher: dispatcher.as_ref(),
                    geometry: geometry.clone(),
                    snapshot: Snapshot {
                        child: child_snapshot,
                        terminal: state,
                        viewport: geometry.child,
                        outer: geometry.outer,
                        observed_at: SystemTime::now(),
                    },
                    master: Some(pair.master),
                    fd,
                    wait: receive,
                    waiter: Some(waiter),
                    resized,
                    router: Router::new(),
                    compositor: Compositor::new(),
                    queue: VecDeque::new(),
                    queued: 0,
                    held: VecDeque::from(saved),
                    waited: false,
                    eof: false,
                    started: false,
                    state_pending: false,
                    composed: false,
                    metadata_dirty: true,
                    scroll_dirty: false,
                    render_due: None,
                    escape_due: None,
                    hold_due: None,
                    kill_due: None,
                    reap_due: None,
                    drain_due: None,
                    rendered_at: None,
                    drain_error: None,
                    terminating: false,
                };
                let run = (|| -> Result<()> {
                    let flags = transport::cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
                    transport::cvt(unsafe {
                        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK)
                    })?;
                    session.console.enter()?;
                    session.render(true)?;
                    session.emit_snapshot(EventKind::Started)?;
                    session.started = true;
                    session.route_held()?;
                    if session.console.framer.needs_deadline() {
                        session.escape_due = Some(Instant::now() + ESCAPE_TIMEOUT);
                    }
                    session.run_loop(cancel)
                })();
                self.close();
                let cleanup = session.cleanup();
                result.process_state = session.snapshot.child.process_state.clone();
                result.drain_error = session.drain_error.take().map(Arc::new);
                result.cleanup_error = cleanup.err().map(Arc::new);
                run
            })();
            self.close();
            if let Err(error) = console.restore() {
                let mut cleanup = result.cleanup_error.take().map(Error::Shared);
                crate::append_error(&mut cleanup, error);
                result.cleanup_error = cleanup.map(Arc::new);
            }
            drop(signal_guard);
            run
        })();
        if let Err(error) = execute {
            failure = Some(error);
        }
        self.close();
        if let Some(dispatcher) = &mut dispatcher {
            dispatcher.close();
            if failure.is_none() {
                failure = dispatcher.failure().map(Error::CallbackPanic);
            }
        }
        if let Some(error) = &result.cleanup_error {
            crate::append_error(&mut failure, Error::Shared(error.clone()));
        }
        match failure {
            Some(error) => Err(RunError { error, result }),
            None => Ok(result),
        }
    }
}

struct SignalGuard {
    handle: signal_hook::iterator::Handle,
    thread: Option<JoinHandle<()>>,
}
impl Drop for SignalGuard {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn terminal_state_initial(mut terminal: Terminal) -> Result<(Terminal, State)> {
    let state = terminal.snapshot()?;
    Ok((terminal, state))
}
fn emit(dispatcher: Option<&Dispatcher>, event: Event) -> Result<()> {
    if let Some(dispatcher) = dispatcher {
        dispatcher.emit(event)
    } else {
        Ok(())
    }
}
fn pty_size(geometry: &Geometry, console: &Console<'_>) -> PtySize {
    PtySize {
        cols: geometry.child.width,
        rows: geometry.child.height,
        pixel_width: (u64::from(geometry.child.width) * u64::from(console.cell_width))
            .min(u64::from(u16::MAX)) as u16,
        pixel_height: (u64::from(geometry.child.height) * u64::from(console.cell_height))
            .min(u64::from(u16::MAX)) as u16,
    }
}
fn pty_snapshot(fd: i32) -> PtySnapshot {
    let window = transport::window(fd);
    let settings = transport::settings(fd);
    let mut group = 0;
    let foreground =
        transport::cvt(unsafe { libc::ioctl(fd, libc::TIOCGPGRP, &mut group) }).map(|_| group);
    PtySnapshot {
        window: window.as_ref().ok().copied(),
        settings: settings.as_ref().ok().copied(),
        foreground_group: process::Field::observe("TIOCGPGRP on owned PTY", foreground),
        window_error: window.err().map(Arc::new),
        settings_error: settings.err().map(Arc::new),
        observed_at: SystemTime::now(),
    }
}
struct Pending {
    bytes: Vec<u8>,
    offset: usize,
    origin: String,
}
struct Session<'f, 'c, 'tty, T> {
    frame: &'f Frame<'tty, T>,
    console: &'c mut Console<'tty>,
    terminal: Terminal,
    dispatcher: Option<&'f Dispatcher>,
    geometry: Geometry,
    snapshot: Snapshot,
    master: Option<Box<dyn MasterPty + Send>>,
    fd: i32,
    wait: mpsc::Receiver<io::Result<portable_pty::ExitStatus>>,
    waiter: Option<JoinHandle<()>>,
    resized: Arc<AtomicBool>,
    router: Router,
    compositor: Compositor,
    queue: VecDeque<Pending>,
    queued: usize,
    held: VecDeque<Packet>,
    waited: bool,
    eof: bool,
    started: bool,
    state_pending: bool,
    composed: bool,
    metadata_dirty: bool,
    scroll_dirty: bool,
    render_due: Option<Instant>,
    escape_due: Option<Instant>,
    hold_due: Option<Instant>,
    kill_due: Option<Instant>,
    reap_due: Option<Instant>,
    drain_due: Option<Instant>,
    rendered_at: Option<Instant>,
    drain_error: Option<Error>,
    terminating: bool,
}
impl<T: Clone + Send + 'static> Session<'_, '_, '_, T> {
    fn event(&self, event: Event) -> Result<()> {
        emit(self.dispatcher, event)
    }
    fn wants(&self, kind: EventKind) -> bool {
        self.dispatcher.is_some_and(|d| d.wants(kind))
    }
    fn emit_snapshot(&self, kind: EventKind) -> Result<()> {
        let mut event = Event::new(kind);
        if self.wants(kind) {
            event.snapshot = Some(self.snapshot.clone());
        }
        self.event(event)
    }
    fn collect_metadata(&mut self) {
        let inventory = process::list(self.snapshot.child.pid);
        self.snapshot.child.session_error = inventory.error.map(Arc::new);
        self.snapshot.child.session_processes = inventory.items;
        if !self.waited {
            self.snapshot.child.operating_system = self
                .snapshot
                .child
                .session_processes
                .iter()
                .find(|p| p.pid == self.snapshot.child.pid)
                .cloned()
                .unwrap_or_else(|| process::read(self.snapshot.child.pid));
        }
        if self.fd >= 0 {
            self.snapshot.child.pty = pty_snapshot(self.fd);
        }
    }
    fn capture_state(&mut self) -> Result<()> {
        let state = self.terminal.snapshot()?;
        self.metadata_dirty |= metadata_changed(&self.snapshot.terminal, &state);
        self.scroll_dirty |= self.snapshot.terminal.scrollbar != state.scrollbar
            || self.snapshot.terminal.scrollback_rows != state.scrollback_rows
            || self.snapshot.terminal.total_rows != state.total_rows;
        self.snapshot.terminal = state;
        self.snapshot.observed_at = SystemTime::now();
        self.state_pending = false;
        Ok(())
    }
    fn sync_input(&mut self) -> Result<()> {
        let input = self.terminal.input_state()?;
        self.console.sync_input(&input)?;
        if input.held {
            self.hold_due.get_or_insert(Instant::now() + HOLD_TIMEOUT);
        } else {
            self.hold_due = None;
        }
        Ok(())
    }
    fn render(&mut self, lifecycle: bool) -> Result<()> {
        if self.state_pending {
            self.capture_state()?;
        }
        self.sync_input()?;
        let (configured, invalidated) = self.frame.region_demand();
        let interested = configured || self.dispatcher.is_some_and(Dispatcher::wants_snapshots);
        if lifecycle
            || interested
                && (self.metadata_dirty
                    || invalidated
                    || self.scroll_dirty
                        && self
                            .snapshot
                            .child
                            .pty
                            .observed_at
                            .elapsed()
                            .unwrap_or_default()
                            >= HOLD_TIMEOUT)
        {
            self.collect_metadata();
        }
        self.compositor.paint(
            self.frame,
            self.console,
            &self.snapshot,
            &self.geometry,
            Damage {
                full: !self.composed,
                regions: lifecycle || self.metadata_dirty || self.scroll_dirty,
                rows: self.terminal.dirty_rows(),
            },
        )?;
        self.terminal.clear_damage();
        self.composed = true;
        self.metadata_dirty = false;
        self.scroll_dirty = false;
        self.render_due = None;
        self.rendered_at = Some(Instant::now());
        Ok(())
    }
    fn schedule_render(&mut self) -> Result<()> {
        if self.render_due.is_some() {
            return Ok(());
        }
        let due = self
            .rendered_at
            .map(|at| at + FRAME_INTERVAL)
            .unwrap_or_else(Instant::now);
        if due <= Instant::now() {
            self.render(false)
        } else {
            self.render_due = Some(due);
            Ok(())
        }
    }
    fn geometry(&mut self, geometry: Geometry) -> Result<()> {
        self.terminal.resize(
            geometry.child.width,
            geometry.child.height,
            self.console.cell_width,
            self.console.cell_height,
        )?;
        self.master
            .as_ref()
            .expect("owned PTY")
            .resize(pty_size(&geometry, self.console))
            .map_err(|error| {
                Error::Pty(error.reallocate_into_boxed_dyn_error_without_backtrace())
            })?;
        self.geometry = geometry;
        self.snapshot.outer = self.geometry.outer;
        self.snapshot.viewport = self.geometry.child;
        self.composed = false;
        self.state_pending = true;
        self.effects()?;
        self.render(true)?;
        self.emit_snapshot(EventKind::Resized)
    }
    fn update_layout(&mut self) -> Result<()> {
        let changed = {
            let mut c = self.frame.config.lock().unwrap();
            std::mem::take(&mut c.layout_dirty)
        };
        if changed {
            let next = self
                .frame
                .layout(self.geometry.outer.width, self.geometry.outer.height)?;
            if next != self.geometry {
                self.geometry(next)?;
            }
        }
        Ok(())
    }
    fn resize(&mut self) -> Result<()> {
        let size = self.console.window()?;
        let geometry = self.frame.layout(size.ws_col, size.ws_row)?;
        let width = u32::from(size.ws_xpixel) / u32::from(size.ws_col);
        let height = u32::from(size.ws_ypixel) / u32::from(size.ws_row);
        if geometry == self.geometry
            && width == self.console.cell_width
            && height == self.console.cell_height
        {
            return Ok(());
        }
        self.console.cell_width = width;
        self.console.cell_height = height;
        if width == 0 || height == 0 {
            self.console.buffer.extend(b"\x1b[16t");
        }
        self.geometry(geometry)
    }
    fn enqueue(&mut self, bytes: &[u8], origin: &str) -> Result<()> {
        if bytes.is_empty() || self.waited {
            return Ok(());
        }
        if bytes.len() > QUEUE_LIMIT - self.queued {
            return Err(Error::Session(format!(
                "child input exceeded {QUEUE_LIMIT}-byte queue"
            )));
        }
        if let Some(last) = self.queue.back_mut().filter(|p| p.origin == origin) {
            if last.offset > 0 {
                last.bytes.drain(..last.offset);
                last.offset = 0;
            }
            last.bytes.extend(bytes);
        } else {
            self.queue.push_back(Pending {
                bytes: bytes.to_vec(),
                offset: 0,
                origin: origin.into(),
            });
        }
        self.queued += bytes.len();
        Ok(())
    }
    fn route_held(&mut self) -> Result<bool> {
        let mut routed = false;
        while self.held.front().is_some_and(|p| {
            self.waited || Router::max_output_size(p).min(QUEUE_LIMIT) <= QUEUE_LIMIT - self.queued
        }) {
            let packet = self.held.pop_front().unwrap();
            self.route(packet)?;
            routed = true;
        }
        if routed && self.held.is_empty() && self.escape_due.is_some() {
            self.escape_due = Some(Instant::now() + ESCAPE_TIMEOUT);
        }
        Ok(routed)
    }
    fn route(&mut self, packet: Packet) -> Result<()> {
        self.update_layout()?;
        let old = (self.console.cell_width, self.console.cell_height);
        if self.console.consume_reply(&packet) {
            if old != (self.console.cell_width, self.console.cell_height) {
                self.geometry(self.geometry.clone())?;
            }
            return self.sync_input();
        }
        self.router.host = HostProfile {
            kitty_flags: self.console.kitty_flags,
            mouse_pixels: self.console.mouse_pixels(),
            cell_width_px: self.console.cell_width,
            cell_height_px: self.console.cell_height,
        };
        let capture = self.frame.config.lock().unwrap().capture.clone();
        let routed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(capture) = capture {
                let mut capture = capture.lock().unwrap();
                self.router.route(
                    &packet,
                    &mut self.terminal,
                    self.geometry.child,
                    Some(&mut *capture),
                )
            } else {
                self.router
                    .route(&packet, &mut self.terminal, self.geometry.child, None)
            }
        }))
        .map_err(|panic| Error::CallbackPanic(crate::observation::panic_message(&panic)))??;
        self.update_layout()?;
        if routed.captured {
            let mut event = Event::new(EventKind::Captured);
            event.bytes = packet.raw;
            event.input = match packet.event {
                InputEvent::Key(key) => Some(key),
                _ => None,
            };
            event.disposition = Capture::Consume;
            event.origin = origin(routed.origin).into();
            return self.event(event);
        }
        if routed.bytes.is_empty() || routed.origin != Origin::User {
            let mut event = Event::new(EventKind::Routed);
            event.bytes = packet.raw;
            event.origin = origin(routed.origin).into();
            event.error = routed.error;
            self.event(event)?;
        }
        self.enqueue(&routed.bytes, origin(routed.origin))
    }
    fn write_input(&mut self) -> Result<()> {
        let Some(pending) = self.queue.front_mut() else {
            return Ok(());
        };
        let count = match transport::write(self.fd, &pending.bytes[pending.offset..]) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let mut event = Event::new(EventKind::ChildInput);
        event.bytes = pending.bytes[pending.offset..pending.offset + count].to_vec();
        event.origin = pending.origin.clone();
        pending.offset += count;
        self.queued -= count;
        let finished = pending.offset == pending.bytes.len();
        if finished {
            self.queue.pop_front();
        }
        self.event(event)
    }
    fn effects(&mut self) -> Result<()> {
        for effect in self.terminal.take_effects() {
            if matches!(&effect,Effect::Reply(bytes) if !self.console.kitty_supported()&&kitty_reply(bytes))
            {
                continue;
            }
            if let Effect::Reply(bytes) = &effect {
                self.enqueue(bytes, "terminal-reply")?;
            }
            if self.wants(EventKind::Protocol) {
                let mut event = Event::new(EventKind::Protocol);
                event.origin = effect_origin(&effect).into();
                event.effect = Some(effect.clone());
                self.event(event)?;
            }
            if effect == Effect::Bell {
                self.console.buffer.push(7);
            }
        }
        Ok(())
    }
    fn read_child(&mut self, buffer: &mut [u8]) -> Result<()> {
        let count = match transport::read(self.fd, buffer) {
            Ok(0) => {
                self.eof = true;
                return Ok(());
            }
            Ok(count) => count,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                self.eof = true;
                return Ok(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let mut event = Event::new(EventKind::ChildOutput);
        event.bytes = buffer[..count].to_vec();
        event.origin = "pty".into();
        self.event(event)?;
        self.terminal.write(&buffer[..count])?;
        self.effects()?;
        self.sync_input()?;
        self.console.flush()?;
        self.state_pending = true;
        self.render_due
            .get_or_insert(Instant::now() + FRAME_INTERVAL);
        if self.wants(EventKind::StateChanged) {
            self.capture_state()?;
            self.emit_snapshot(EventKind::StateChanged)?;
        }
        Ok(())
    }
    fn read_outer(&mut self, buffer: &mut [u8]) -> Result<()> {
        let count = match transport::read(self.console.input.as_raw_fd(), buffer) {
            Ok(0) => return Err(Error::Session("outer terminal input ended".into())),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut event = Event::new(EventKind::OuterInput);
        event.bytes = buffer[..count].to_vec();
        event.origin = "terminal".into();
        self.event(event)?;
        self.held.extend(
            self.console
                .framer
                .feed(&buffer[..count])
                .map_err(|error| Error::Session(error.to_string()))?,
        );
        self.route_held()?;
        self.escape_due = self
            .console
            .framer
            .needs_deadline()
            .then(|| Instant::now() + ESCAPE_TIMEOUT);
        self.console.flush()
    }
    fn record_reap(&mut self, status: io::Result<portable_pty::ExitStatus>) -> Result<()> {
        self.waited = true;
        self.reap_due = None;
        self.snapshot.child.process_state = Some(status?);
        Ok(())
    }
    fn reap(&mut self) -> Result<()> {
        if self.waited {
            return Ok(());
        }
        match self.wait.try_recv() {
            Ok(status) => {
                self.record_reap(status)?;
                self.queue.clear();
                self.queued = 0;
                self.collect_metadata();
                self.drain_due = Some(Instant::now() + DRAIN_TIMEOUT);
                self.route_held()?;
                if self.state_pending && self.wants(EventKind::Exited) {
                    self.capture_state()?;
                }
                if self.started {
                    self.emit_snapshot(EventKind::Exited)?;
                }
                Ok(())
            }
            Err(mpsc::TryRecvError::Empty) => Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(Error::Session("child wait channel disconnected".into()))
            }
        }
    }
    fn signal_groups(&mut self, signals: &[i32]) -> Result<()> {
        let inventory = process::groups(self.snapshot.child.pid);
        let mut groups = inventory.items;
        let mut error = inventory.error.map(Error::from);
        if error.is_some() && !self.waited && !groups.contains(&self.snapshot.child.pid) {
            groups.push(self.snapshot.child.pid);
        }
        for signal in signals {
            for &group in &groups {
                if unsafe { libc::kill(-group, *signal) } == 0 {
                    continue;
                }
                let next = io::Error::last_os_error();
                if next.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                if next.raw_os_error() == Some(libc::EPERM)
                    && process::group_exited(group).unwrap_or(false)
                {
                    continue;
                }
                crate::append_error(&mut error, next.into());
            }
        }
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn kill(&mut self) -> Result<()> {
        if !self.waited && self.reap_due.is_none() {
            self.reap_due = Some(Instant::now() + DRAIN_TIMEOUT);
        }
        self.signal_groups(&[libc::SIGKILL])
    }
    fn deadlines(&mut self) -> Result<()> {
        if expired(self.render_due) {
            self.render(false)?;
        }
        if expired(self.escape_due) && self.held.is_empty() {
            self.escape_due = None;
            if let Some(packet) = self.console.framer.flush() {
                self.held.push_back(packet);
                self.route_held()?;
            }
        }
        if expired(self.hold_due) {
            self.hold_due = None;
            self.terminal.release_hold()?;
            self.effects()?;
            self.state_pending = true;
            self.render(false)?;
        }
        if expired(self.kill_due) {
            self.kill_due = None;
            self.kill()?;
        }
        if expired(self.drain_due) && !self.eof {
            let message = "child output did not close before drain deadline";
            self.drain_error = Some(Error::Session(message.into()));
            return Err(Error::Session(message.into()));
        }
        Ok(())
    }
    fn poll_timeout(&self) -> i32 {
        let escape = if self.held.is_empty() {
            self.escape_due
        } else {
            None
        };
        [
            escape,
            self.hold_due,
            self.kill_due,
            self.reap_due,
            self.drain_due,
            self.render_due,
        ]
        .into_iter()
        .flatten()
        .map(|due| {
            due.saturating_duration_since(Instant::now())
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32
        })
        .min()
        .unwrap_or(-1)
    }
    fn run_loop(&mut self, cancel: &Cancellation) -> Result<()> {
        let mut failure = None;
        let mut bytes = vec![0; READ_SIZE];
        let mut hangup = hangup::HangupWatch::default();
        loop {
            if let Err(error) = self.reap() {
                crate::append_error(&mut failure, error);
            }
            if cancel.is_cancelled() {
                failure.get_or_insert(Error::Cancelled);
            }
            if let Some(error) = self.dispatcher.and_then(Dispatcher::failure) {
                failure.get_or_insert(Error::CallbackPanic(error));
            }
            if self.waited && self.eof {
                if let Err(error) = self.render(false) {
                    crate::append_error(&mut failure, error);
                }
                return match failure {
                    Some(error) => Err(error),
                    None => Ok(()),
                };
            }
            if let Some(error) = self.frame.config.lock().unwrap().config_error.take() {
                failure.get_or_insert(error);
            }
            if failure.is_some() && !self.terminating {
                self.frame.close();
                self.terminating = true;
                if let Err(error) = self.signal_groups(&[libc::SIGTERM, libc::SIGCONT]) {
                    crate::append_error(&mut failure, error);
                }
                self.kill_due = Some(Instant::now() + Duration::from_secs(1));
            }
            if let Err(error) = self.deadlines() {
                crate::append_error(&mut failure, error);
                break;
            }
            if !self.waited && expired(self.reap_due) {
                break;
            }
            let accepting = failure.is_none() && !self.waited;
            match hangup.watch(
                self.console.input.as_raw_fd(),
                accepting && !self.held.is_empty(),
            ) {
                Ok(false) => (),
                Ok(true) => {
                    failure.get_or_insert(Error::Session("outer terminal disconnected".into()));
                    continue;
                }
                Err(error) => {
                    failure.get_or_insert(error.into());
                    continue;
                }
            }
            let mut fds = [
                libc::pollfd {
                    fd: self.frame.wake.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if accepting {
                        self.console.input.as_raw_fd()
                    } else {
                        -1
                    },
                    events: if self.held.is_empty() {
                        libc::POLLIN
                    } else {
                        0
                    },
                    revents: 0,
                },
                libc::pollfd {
                    fd: if self.eof { -1 } else { self.fd },
                    events: libc::POLLIN
                        | if !self.queue.is_empty() && accepting {
                            libc::POLLOUT
                        } else {
                            0
                        },
                    revents: 0,
                },
                hangup.poll_fd(),
            ];
            if let Err(error) = transport::poll(&mut fds, self.poll_timeout()) {
                crate::append_error(&mut failure, error.into());
                break;
            }
            let ready = (|| -> Result<()> {
                if fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    return Err(Error::Session(
                        self.frame
                            .wake
                            .failure()
                            .unwrap_or_else(|| "terminal wake pipe disconnected".into()),
                    ));
                }
                if fds[0].revents & libc::POLLIN != 0 {
                    self.frame.wake.drain()?;
                    if self.resized.swap(false, Ordering::AcqRel) {
                        self.resize()?;
                    }
                    self.update_layout()?;
                    self.schedule_render()?;
                }
                if fds[2].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 && !self.eof
                {
                    self.read_child(&mut bytes)?;
                }
                if fds[1].revents & libc::POLLIN != 0 && accepting {
                    self.read_outer(&mut bytes)?;
                }
                if fds[1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
                    && accepting
                {
                    return Err(Error::Session("outer terminal disconnected".into()));
                }
                if fds[3].revents != 0 && hangup.hung_up()? {
                    return Err(Error::Session("outer terminal disconnected".into()));
                }
                if fds[2].revents & libc::POLLOUT != 0 && accepting {
                    self.write_input()?;
                    if self.route_held()? {
                        self.console.flush()?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = ready {
                crate::append_error(&mut failure, error);
                if self.terminating {
                    break;
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn cleanup(&mut self) -> Result<()> {
        let was_waited = self.waited;
        let mut failure = self.kill().err();
        let mut bytes = vec![0; READ_SIZE];
        while !self.waited && !expired(self.reap_due) {
            match self.wait.try_recv() {
                Ok(status) => {
                    if let Err(error) = self.record_reap(status) {
                        crate::append_error(&mut failure, error);
                    }
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    crate::append_error(
                        &mut failure,
                        Error::Session("child wait channel disconnected".into()),
                    );
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
            let mut fds = [
                libc::pollfd {
                    fd: self.frame.wake.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if self.eof { -1 } else { self.fd },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if let Err(error) = transport::poll(
                &mut fds,
                self.reap_due
                    .map(|due| {
                        due.saturating_duration_since(Instant::now())
                            .as_millis()
                            .saturating_add(1)
                            .min(50) as i32
                    })
                    .unwrap_or(50),
            ) {
                crate::append_error(&mut failure, error.into());
                break;
            }
            if fds[0].revents & libc::POLLIN != 0
                && let Err(error) = self.frame.wake.drain()
            {
                crate::append_error(&mut failure, error.into());
            }
            if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                match transport::read(self.fd, &mut bytes) {
                    Ok(0) => self.eof = true,
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => self.eof = true,
                    Err(error)
                        if !matches!(
                            error.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        crate::append_error(&mut failure, error.into());
                        break;
                    }
                    _ => (),
                }
            }
        }
        if !self.waited && self.wants(EventKind::Exited) {
            self.snapshot.child.pty = pty_snapshot(self.fd);
        }
        self.master.take();
        self.fd = -1;
        if !self.waited {
            match self.wait.recv() {
                Ok(status) => {
                    if let Err(error) = self.record_reap(status) {
                        crate::append_error(&mut failure, error);
                    }
                }
                Err(error) => {
                    crate::append_error(&mut failure, Error::Session(error.to_string()));
                }
            }
        }
        if let Some(waiter) = self.waiter.take() {
            let _ = waiter.join();
        }
        if !was_waited && self.started {
            self.collect_metadata();
            if self.state_pending
                && let Err(error) = self.capture_state()
            {
                crate::append_error(&mut failure, error);
            }
            if let Err(error) = self.emit_snapshot(EventKind::Exited) {
                crate::append_error(&mut failure, error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|due| due <= Instant::now())
}
fn kitty_reply(bytes: &[u8]) -> bool {
    bytes
        .strip_prefix(b"\x1b[?")
        .and_then(|v| v.strip_suffix(b"u"))
        .is_some_and(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit))
}
fn metadata_changed(a: &State, b: &State) -> bool {
    a.title != b.title
        || a.directory != b.directory
        || a.input.cols != b.input.cols
        || a.input.rows != b.input.rows
        || a.input.cell_width_px != b.input.cell_width_px
        || a.input.cell_height_px != b.input.cell_height_px
        || a.input.alternate != b.input.alternate
        || a.input
            .modes
            .iter()
            .filter(|(m, _)| !matches!(m, 12 | 25 | 2026))
            .ne(b
                .input
                .modes
                .iter()
                .filter(|(m, _)| !matches!(m, 12 | 25 | 2026)))
        || a.input.kitty_flags != b.input.kitty_flags
        || a.input.modify_other_keys_2 != b.input.modify_other_keys_2
        || a.input.mouse_tracking != b.input.mouse_tracking
        || a.input.mouse_tracking_mode != b.input.mouse_tracking_mode
        || a.mouse_shape != b.mouse_shape
        || a.scrollback_max_bytes != b.scrollback_max_bytes
        || a.scrollback_max_lines != b.scrollback_max_lines
}
fn origin(origin: Origin) -> &'static str {
    match origin {
        Origin::User => "user-input",
        Origin::Captured => "user-input",
        Origin::Paste => "paste-envelope",
        Origin::Focus => "focus-input",
        Origin::Mouse => "mouse-protocol",
        Origin::OutsideChild => "outside-child",
        Origin::UnmeasuredPixels => "unmeasured-pixel-mouse",
        Origin::UnownedGesture => "unowned-mouse-gesture",
        Origin::MouseDisabled => "mouse-disabled",
        Origin::Unroutable => "mouse-protocol",
    }
}
fn effect_origin(effect: &Effect) -> &'static str {
    match effect {
        Effect::Reply(_) => "reply",
        Effect::Bell => "bell",
        Effect::Title(_) => "title",
        Effect::Directory(_) => "directory",
        Effect::Progress { .. } => "progress",
        Effect::Notification { .. } => "notification",
        Effect::Unknown { .. } => "unknown",
        Effect::RenderHold(_) => "render-hold",
        Effect::SemanticPrompt { .. } => "semantic-prompt",
        Effect::Reset => "reset",
        Effect::ClipboardRead(_) => "clipboard-read",
        Effect::ClipboardWrite(_) => "clipboard-write",
    }
}
