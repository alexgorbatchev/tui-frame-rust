//! A single child session, with independently invalidated Ratatui regions.
use crate::{
    Error, Result,
    control::{Cancellation, Wake},
    input::Key,
    process,
    routing::Capture,
    terminal::{Effect, State},
};
use portable_pty::{CommandBuilder, ExitStatus};
use ratatui::{buffer::Buffer, layout::Rect};
use std::{
    ffi::OsString,
    os::fd::BorrowedFd,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Header,
    Footer,
    Left,
    Right,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub outer: Rect,
    pub child: Rect,
    pub border: Rect,
    pub regions: [Rect; 4],
}

pub struct DrawContext<'a, T> {
    pub term: Snapshot,
    pub view: &'a mut Buffer,
    pub data: T,
}
type Draw<T> = Arc<Mutex<dyn for<'a> FnMut(DrawContext<'a, T>) + Send>>;
type CaptureHandler = Arc<Mutex<dyn FnMut(&Key) -> Capture + Send>>;
pub(crate) type Observer = Box<dyn FnMut(Event) + Send>;
pub(crate) struct Region<T> {
    pub size: u16,
    pub draw: Option<Draw<T>>,
    pub data: T,
    pub dirty: bool,
    pub canvas: Option<Buffer>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Configuring,
    Running,
    Closed,
}
pub(crate) struct Configuration<T> {
    pub command: CommandBuilder,
    pub regions: [Region<T>; 4],
    pub border: bool,
    pub layout_dirty: bool,
    pub inherit: bool,
    pub capture: Option<CaptureHandler>,
    pub observer: Option<Observer>,
    pub observed: std::collections::BTreeSet<EventKind>,
    pub phase: Phase,
    pub config_error: Option<Error>,
    pub probe_timeout: Duration,
}
pub struct Frame<'a, T> {
    pub(crate) config: Arc<Mutex<Configuration<T>>>,
    pub(crate) terminal: Arc<Mutex<(BorrowedFd<'a>, BorrowedFd<'a>)>>,
    pub(crate) wake: Arc<Wake>,
}
impl<T> Clone for Frame<'_, T> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            terminal: self.terminal.clone(),
            wake: self.wake.clone(),
        }
    }
}
/// A lifetime-independent, non-owning handle suitable for capture and observer callbacks.
pub struct Controller<T> {
    config: Weak<Mutex<Configuration<T>>>,
    wake: Weak<Wake>,
}
impl<T> Clone for Controller<T> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            wake: self.wake.clone(),
        }
    }
}
impl<T> Controller<T> {
    pub fn set_border(&self, enabled: bool) -> Result<()> {
        let state = self.config.upgrade().ok_or(Error::SessionClosed)?;
        let mut c = state.lock().unwrap();
        if c.phase == Phase::Closed {
            return Err(Error::SessionClosed);
        }
        if c.border != enabled {
            c.border = enabled;
            c.layout_dirty = true;
            if let Some(wake) = self.wake.upgrade() {
                wake.notify();
            }
        }
        Ok(())
    }
    pub fn invalidate(&self, edge: Edge, data: T) -> Result<()> {
        let state = self.config.upgrade().ok_or(Error::SessionClosed)?;
        let mut c = state.lock().unwrap();
        if c.phase == Phase::Closed {
            return Err(Error::SessionClosed);
        }
        let region = &mut c.regions[edge as usize];
        if region.draw.is_none() {
            return Err(Error::RegionNotConfigured);
        }
        region.data = data;
        region.dirty = true;
        if let Some(wake) = self.wake.upgrade() {
            wake.notify();
        }
        Ok(())
    }
}

impl<'a, T: Clone + Send + 'static> Frame<'a, T> {
    pub fn new(command: CommandBuilder, initial: T) -> Self {
        Self::try_new(command, initial).expect("creating frame wake pipe")
    }
    pub fn try_new(command: CommandBuilder, initial: T) -> Result<Self> {
        Ok(Self {
            config: Arc::new(Mutex::new(Configuration {
                command,
                regions: std::array::from_fn(|_| Region {
                    size: 0,
                    draw: None,
                    data: initial.clone(),
                    dirty: false,
                    canvas: None,
                }),
                border: false,
                layout_dirty: false,
                inherit: true,
                capture: None,
                observer: None,
                observed: EventKind::all().into_iter().collect(),
                phase: Phase::Configuring,
                config_error: None,
                probe_timeout: Duration::from_millis(300),
            })),
            terminal: Arc::new(Mutex::new((
                unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) },
                unsafe { BorrowedFd::borrow_raw(libc::STDOUT_FILENO) },
            ))),
            wake: Wake::new()?,
        })
    }
    pub fn region(
        &self,
        edge: Edge,
        size: u16,
        draw: impl for<'v> FnMut(DrawContext<'v, T>) + Send + 'static,
    ) -> Result<()> {
        let mut config = self.config.lock().unwrap();
        self.configurable(&mut config)?;
        if size == 0 {
            return Err(Error::Session(
                "region requires positive dimensions and a drawing callback".into(),
            ));
        }
        let region = &mut config.regions[edge as usize];
        region.size = size;
        region.draw = Some(Arc::new(Mutex::new(draw)));
        region.dirty = true;
        Ok(())
    }
    pub fn header(
        &self,
        size: u16,
        draw: impl for<'v> FnMut(DrawContext<'v, T>) + Send + 'static,
    ) -> Result<()> {
        self.region(Edge::Header, size, draw)
    }
    pub fn footer(
        &self,
        size: u16,
        draw: impl for<'v> FnMut(DrawContext<'v, T>) + Send + 'static,
    ) -> Result<()> {
        self.region(Edge::Footer, size, draw)
    }
    pub fn left(
        &self,
        size: u16,
        draw: impl for<'v> FnMut(DrawContext<'v, T>) + Send + 'static,
    ) -> Result<()> {
        self.region(Edge::Left, size, draw)
    }
    pub fn right(
        &self,
        size: u16,
        draw: impl for<'v> FnMut(DrawContext<'v, T>) + Send + 'static,
    ) -> Result<()> {
        self.region(Edge::Right, size, draw)
    }
    fn configurable(&self, config: &mut Configuration<T>) -> Result<()> {
        if config.phase != Phase::Configuring {
            config.config_error = Some(Error::ConfigurationFrozen);
            self.wake.notify();
            return Err(Error::ConfigurationFrozen);
        }
        Ok(())
    }
    pub fn terminal(&self, input: BorrowedFd<'a>, output: BorrowedFd<'a>) -> Result<()> {
        let mut c = self.config.lock().unwrap();
        self.configurable(&mut c)?;
        *self.terminal.lock().unwrap() = (input, output);
        Ok(())
    }
    pub fn controller(&self) -> Controller<T> {
        Controller {
            config: Arc::downgrade(&self.config),
            wake: Arc::downgrade(&self.wake),
        }
    }
    pub fn inherit_terminal(&self, inherit: bool) -> Result<()> {
        let mut c = self.config.lock().unwrap();
        self.configurable(&mut c)?;
        c.inherit = inherit;
        Ok(())
    }
    pub fn capture(&self, handler: impl FnMut(&Key) -> Capture + Send + 'static) -> Result<()> {
        let mut c = self.config.lock().unwrap();
        self.configurable(&mut c)?;
        c.capture = Some(Arc::new(Mutex::new(handler)));
        Ok(())
    }
    pub fn observe(&self, handler: impl FnMut(Event) + Send + 'static) -> Result<()> {
        self.observe_events(EventKind::all(), handler)
    }
    pub fn observe_events(
        &self,
        kinds: impl IntoIterator<Item = EventKind>,
        handler: impl FnMut(Event) + Send + 'static,
    ) -> Result<()> {
        let mut c = self.config.lock().unwrap();
        self.configurable(&mut c)?;
        c.observer = Some(Box::new(handler));
        c.observed = kinds.into_iter().collect();
        Ok(())
    }
    pub fn set_border(&self, enabled: bool) -> Result<()> {
        self.controller().set_border(enabled)
    }
    pub fn invalidate(&self, edge: Edge, data: T) -> Result<()> {
        self.controller().invalidate(edge, data)
    }
    pub fn layout(&self, cols: u16, rows: u16) -> Result<Geometry> {
        if cols == 0 || rows == 0 || usize::from(cols) * usize::from(rows) > 1 << 20 {
            return Err(Error::ViewportTooSmall);
        }
        let c = self.config.lock().unwrap();
        let [top, bottom, left, right] = c.regions.each_ref().map(|r| r.size);
        let margin = u16::from(c.border);
        let w = cols
            .checked_sub(left)
            .and_then(|n| n.checked_sub(right))
            .and_then(|n| n.checked_sub(2 * margin))
            .filter(|n| *n > 0)
            .ok_or(Error::ViewportTooSmall)?;
        let h = rows
            .checked_sub(top)
            .and_then(|n| n.checked_sub(bottom))
            .and_then(|n| n.checked_sub(2 * margin))
            .filter(|n| *n > 0)
            .ok_or(Error::ViewportTooSmall)?;
        Ok(Geometry {
            outer: Rect::new(0, 0, cols, rows),
            child: Rect::new(left + margin, top + margin, w, h),
            border: Rect::new(left, top, w + 2 * margin, h + 2 * margin),
            regions: [
                Rect::new(0, 0, cols, top),
                Rect::new(0, rows - bottom, cols, bottom),
                Rect::new(0, top, left, rows - top - bottom),
                Rect::new(cols - right, top, right, rows - top - bottom),
            ],
        })
    }
    pub(crate) fn region_demand(&self) -> (bool, bool) {
        let c = self.config.lock().unwrap();
        (
            c.regions.iter().any(|r| r.draw.is_some()),
            c.regions.iter().any(|r| r.dirty),
        )
    }
    pub(crate) fn close(&self) {
        self.config.lock().unwrap().phase = Phase::Closed;
    }
    pub(crate) fn begin(&self, cancel: &Cancellation) -> Result<()> {
        let mut c = self.config.lock().unwrap();
        if c.phase != Phase::Configuring {
            return Err(Error::AlreadyRun);
        }
        c.phase = Phase::Running;
        if let Some(error) = c.config_error.take() {
            c.phase = Phase::Closed;
            return Err(error);
        }
        if c.command.get_argv().first().is_none_or(|a| a.is_empty())
            || c.command.is_default_prog()
            || !c.command.get_controlling_tty()
        {
            c.phase = Phase::Closed;
            return Err(Error::Session(
                "an explicit unstarted command owning its controlling PTY is required".into(),
            ));
        }
        if cancel.is_cancelled() {
            c.phase = Phase::Closed;
            return Err(Error::Cancelled);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct PtySnapshot {
    pub window: Option<libc::winsize>,
    pub settings: Option<libc::termios>,
    pub foreground_group: process::Field<i32>,
    pub window_error: Option<Arc<std::io::Error>>,
    pub settings_error: Option<Arc<std::io::Error>>,
    pub observed_at: SystemTime,
}
#[derive(Clone)]
pub struct ChildSnapshot {
    pub pid: i32,
    pub executable: OsString,
    pub args: Vec<OsString>,
    pub environment: Vec<OsString>,
    pub directory: PathBuf,
    pub started_at: SystemTime,
    pub process_state: Option<ExitStatus>,
    pub operating_system: process::Snapshot,
    pub session_processes: Vec<process::Snapshot>,
    pub session_error: Option<Arc<std::io::Error>>,
    pub pty: PtySnapshot,
}
#[derive(Clone)]
pub struct Snapshot {
    pub child: ChildSnapshot,
    pub terminal: State,
    pub viewport: Rect,
    pub outer: Rect,
    pub observed_at: SystemTime,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventKind {
    OuterInput,
    ChildOutput,
    ChildInput,
    Started,
    Exited,
    Resized,
    Captured,
    StateChanged,
    Protocol,
    Routed,
}
impl EventKind {
    pub fn all() -> [Self; 10] {
        [
            Self::OuterInput,
            Self::ChildOutput,
            Self::ChildInput,
            Self::Started,
            Self::Exited,
            Self::Resized,
            Self::Captured,
            Self::StateChanged,
            Self::Protocol,
            Self::Routed,
        ]
    }
}
#[derive(Clone)]
pub struct Event {
    pub kind: EventKind,
    pub sequence: u64,
    pub offset: u64,
    pub time: SystemTime,
    pub bytes: Vec<u8>,
    pub input: Option<Key>,
    pub snapshot: Option<Snapshot>,
    pub disposition: Capture,
    pub origin: String,
    pub error: Option<String>,
    pub effect: Option<Effect>,
}
impl Event {
    pub(crate) fn new(kind: EventKind) -> Self {
        Self {
            kind,
            sequence: 0,
            offset: 0,
            time: SystemTime::now(),
            bytes: Vec::new(),
            input: None,
            snapshot: None,
            disposition: Capture::Pass,
            origin: String::new(),
            error: None,
            effect: None,
        }
    }
}
#[derive(Default, Debug)]
pub struct RunResult {
    pub process_state: Option<ExitStatus>,
    pub drain_error: Option<Arc<Error>>,
    pub cleanup_error: Option<Arc<Error>>,
}
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct RunError {
    #[source]
    pub error: Error,
    pub result: RunResult,
}
