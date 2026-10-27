//! Borrowed TTY acquisition, bounded capability negotiation and restoration.
use crate::{
    Error, Result,
    ffi::*,
    terminal::{self, InputState, Options, Profile, Rgb, State},
    transport,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    os::fd::{AsRawFd, BorrowedFd},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MODES: &[u16] = &[
    1, 5, 7, 9, 12, 25, 66, 67, 1000, 1001, 1002, 1003, 1004, 1005, 1006, 1007, 1015, 1016, 1035,
    1036, 1039, 1049, 2004, 2026, 2027, 2031,
];
const TRACKING: &[u16] = &[9, 1000, 1001, 1002, 1003];
const FORMATS: &[u16] = &[1005, 1006, 1015, 1016];
const REPLY_LIMIT: usize = 1 + 256 * (5 + 18) + 1;
static OWNERS: OnceLock<Mutex<BTreeSet<u64>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColorProfile {
    None,
    Ansi,
    Ansi256,
    TrueColor,
}

pub struct Console<'a> {
    pub(crate) input: BorrowedFd<'a>,
    pub(crate) output: BorrowedFd<'a>,
    device: u64,
    original: Option<libc::termios>,
    pub inherit: bool,
    pub profile: Profile,
    pub cell_width: u32,
    pub cell_height: u32,
    pub(crate) framer: crate::input::Framer,
    entry: BTreeMap<u16, u8>,
    applied: BTreeMap<u16, bool>,
    pending: BTreeSet<u16>,
    preference_modes: BTreeSet<u16>,
    colors: BTreeSet<u16>,
    palette: BTreeSet<u8>,
    cursor_pending: bool,
    scheme_pending: bool,
    preferences_closed: bool,
    pub kitty_flags: u8,
    kitty_pending: bool,
    kitty_supported: bool,
    kitty_pushed: bool,
    modify_pending: bool,
    modify_supported: bool,
    modify_entry: u8,
    modify_level: u8,
    cell_pending: bool,
    da_pending: bool,
    alternate: bool,
    cursor_entry: Option<u8>,
    cursor_style: Option<u8>,
    cursor_color: Option<Rgb>,
    pub(crate) buffer: Vec<u8>,
    restored: bool,
    color_profile: ColorProfile,
}

impl<'a> Console<'a> {
    pub fn new(input: BorrowedFd<'a>, output: BorrowedFd<'a>, inherit: bool) -> Result<Self> {
        let a = input.as_raw_fd();
        let b = output.as_raw_fd();
        if unsafe { libc::isatty(a) } != 1 || unsafe { libc::isatty(b) } != 1 {
            return Err(Error::Session("input and output must be terminals".into()));
        }
        let mut first: libc::stat = unsafe { std::mem::zeroed() };
        let mut second: libc::stat = unsafe { std::mem::zeroed() };
        transport::cvt(unsafe { libc::fstat(a, &mut first) })?;
        transport::cvt(unsafe { libc::fstat(b, &mut second) })?;
        if first.st_rdev != second.st_rdev {
            return Err(Error::Session(
                "input and output must refer to the same terminal".into(),
            ));
        }
        let device = first.st_rdev as u64;
        if !OWNERS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(device)
        {
            return Err(Error::Session(
                "terminal is already owned by a frame session".into(),
            ));
        }
        let original = match transport::settings(a) {
            Ok(value) => value,
            Err(error) => {
                OWNERS.get().unwrap().lock().unwrap().remove(&device);
                return Err(error.into());
            }
        };
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if let Err(error) = transport::set_settings(a, &raw) {
            OWNERS.get().unwrap().lock().unwrap().remove(&device);
            return Err(error.into());
        }
        Ok(Self {
            input,
            output,
            device,
            original: Some(original),
            inherit,
            profile: Profile::default(),
            cell_width: 0,
            cell_height: 0,
            framer: crate::input::Framer::new(),
            entry: BTreeMap::new(),
            applied: BTreeMap::new(),
            pending: BTreeSet::new(),
            preference_modes: BTreeSet::new(),
            colors: BTreeSet::new(),
            palette: BTreeSet::new(),
            cursor_pending: false,
            scheme_pending: false,
            preferences_closed: false,
            kitty_flags: 0,
            kitty_pending: false,
            kitty_supported: false,
            kitty_pushed: false,
            modify_pending: false,
            modify_supported: false,
            modify_entry: 0,
            modify_level: 0,
            cell_pending: false,
            da_pending: false,
            alternate: false,
            cursor_entry: None,
            cursor_style: None,
            cursor_color: None,
            buffer: Vec::new(),
            restored: false,
            color_profile: detect_color_profile(),
        })
    }

    pub fn window(&self) -> Result<libc::winsize> {
        Ok(transport::window(self.input.as_raw_fd())?)
    }
    pub fn original_settings(&self) -> &libc::termios {
        self.original
            .as_ref()
            .expect("console owns original settings")
    }
    pub fn grapheme_width(&self) -> bool {
        self.switchable(2027) || self.entry.get(&2027) == Some(&3)
    }
    pub fn mouse_pixels(&self) -> bool {
        *self.applied.get(&1016).unwrap_or(&false)
    }
    pub fn synchronized_output(&self) -> bool {
        self.switchable(2026)
    }
    pub fn kitty_supported(&self) -> bool {
        self.kitty_supported
    }
    pub fn color_profile(&self) -> ColorProfile {
        self.color_profile
    }
    pub fn set_color_profile(&mut self, profile: ColorProfile) {
        self.color_profile = profile;
    }
    pub fn flush(&mut self) -> Result<()> {
        transport::write_all(self.output.as_raw_fd(), &self.buffer)?;
        self.buffer.clear();
        Ok(())
    }

    fn queries(&mut self) -> Vec<u8> {
        let mut query = String::new();
        for &mode in MODES {
            self.pending.insert(mode);
            query.push_str(&format!("\x1b[?{mode}$p"));
        }
        self.kitty_pending = true;
        self.modify_pending = true;
        self.cell_pending = true;
        self.da_pending = true;
        query.push_str("\x1b[?u\x1b[?4m\x1b[16t\x1b]12;?\x07\x1bP$q q\x1b\\");
        self.colors.insert(12);
        self.cursor_pending = true;
        if self.inherit {
            self.colors.extend([10, 11]);
            query.push_str("\x1b]10;?\x07\x1b]11;?\x07");
            for index in 0..=255 {
                self.palette.insert(index);
                query.push_str(&format!("\x1b]4;{index};?\x07"));
            }
            for mode in terminal::preference_modes() {
                self.preference_modes.insert(mode);
                if mode & 0x8000 != 0 {
                    query.push_str(&format!("\x1b[{}$p", mode & 0x7fff));
                } else if !self.pending.contains(&mode) {
                    query.push_str(&format!("\x1b[?{mode}$p"));
                }
            }
            self.scheme_pending = true;
            query.push_str("\x1b[?996n");
        }
        query.push_str("\x1b[c");
        query.into_bytes()
    }

    pub fn probe(
        &mut self,
        timeout: Duration,
        mut cancelled: impl FnMut() -> bool,
        mut observe: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<Vec<crate::input::Packet>> {
        let mut query = self.queries();
        let deadline = Instant::now() + timeout;
        let mut flags = transport::Nonblocking::new(self.output)?;
        let mut saved = Vec::new();
        let mut bytes = [0; 4096];
        let outcome = (|| {
            while Instant::now() < deadline
                && (!query.is_empty()
                    || self.pending.contains(&1049)
                    || (self.da_pending && self.awaiting()))
            {
                if cancelled() {
                    return Err(Error::Cancelled);
                }
                let mut fds = [
                    libc::pollfd {
                        fd: self.input.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: if query.is_empty() {
                            -1
                        } else {
                            self.output.as_raw_fd()
                        },
                        events: libc::POLLOUT,
                        revents: 0,
                    },
                ];
                transport::poll(
                    &mut fds,
                    (deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        + 1)
                    .min(50) as i32,
                )?;
                if (fds[0].revents | fds[1].revents)
                    & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                    != 0
                {
                    return Err(Error::Session(
                        "terminal closed during capability negotiation".into(),
                    ));
                }
                if fds[1].revents & libc::POLLOUT != 0 {
                    match transport::write(self.output.as_raw_fd(), &query) {
                        Ok(n) => {
                            query.drain(..n);
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                if fds[0].revents & libc::POLLIN == 0 {
                    continue;
                }
                let n = match transport::read(self.input.as_raw_fd(), &mut bytes) {
                    Ok(0) => {
                        return Err(Error::Session(
                            "terminal input ended during negotiation".into(),
                        ));
                    }
                    Ok(n) => n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                observe(&bytes[..n])?;
                let packets = self
                    .framer
                    .feed(&bytes[..n])
                    .map_err(|e| Error::Session(e.to_string()))?;
                for packet in packets {
                    if !self.consume_reply(&packet) {
                        saved.push(packet);
                    }
                }
            }
            if !query.is_empty() {
                return Err(Error::Session("terminal probe write timed out".into()));
            }
            self.preferences_closed = true;
            if self.entry.get(&1049) != Some(&2) {
                return Err(Error::Session(
                    "terminal must report an inactive alternate screen (DEC mode 1049)".into(),
                ));
            }
            Ok(saved)
        })();
        flags.restore()?;
        outcome
    }

    fn awaiting(&self) -> bool {
        !self.pending.is_empty()
            || self.kitty_pending
            || self.modify_pending
            || self.cell_pending
            || !self.preference_modes.is_empty()
            || !self.colors.is_empty()
            || !self.palette.is_empty()
            || self.cursor_pending
            || self.scheme_pending
    }

    pub fn consume_reply(&mut self, packet: &crate::input::Packet) -> bool {
        if packet.paste {
            return false;
        }
        let raw = &packet.raw;
        let csi = raw
            .strip_prefix(b"\x1b[")
            .or_else(|| raw.strip_prefix(b"\x9b"));
        if let Some(data) = csi {
            if let Some(fields) = data.strip_suffix(b"$y") {
                let (dec, fields) = fields
                    .strip_prefix(b"?")
                    .map_or((false, fields), |v| (true, v));
                let Some((mode, setting)) = split2(fields) else {
                    return false;
                };
                let (Some(mode), Some(setting)) = (number(mode), number(setting)) else {
                    return false;
                };
                let Ok(mode) = u16::try_from(mode) else {
                    return false;
                };
                if setting > 4 {
                    return false;
                }
                let native = if dec { mode } else { mode | 0x8000 };
                let mut consumed = self.preference_modes.remove(&native);
                if consumed && !self.preferences_closed && setting > 0 {
                    self.profile
                        .modes
                        .insert(native, setting == 1 || setting == 3);
                }
                if dec && self.pending.remove(&mode) {
                    self.entry.insert(mode, setting as u8);
                    if setting > 0 {
                        self.applied.insert(mode, setting == 1 || setting == 3);
                    }
                    consumed = true;
                }
                return consumed;
            }
            if let Some(flags) = data
                .strip_prefix(b"?")
                .and_then(|v| v.strip_suffix(b"u"))
                .and_then(number)
            {
                if !self.kitty_pending || flags > 255 {
                    return false;
                }
                self.kitty_pending = false;
                self.kitty_supported = true;
                if self.inherit && !self.preferences_closed {
                    self.profile.kitty_flags = Some(flags as u8);
                }
                return true;
            }
            if let Some(fields) = data.strip_prefix(b"6;").and_then(|v| v.strip_suffix(b"t"))
                && let Some((h, w)) =
                    split2(fields).and_then(|(h, w)| Some((number(h)?, number(w)?)))
            {
                if h > 0 && w > 0 {
                    self.cell_pending = false;
                    self.cell_width = w;
                    self.cell_height = h;
                }
                return true;
            }
            if let Some(level) = data
                .strip_prefix(b">4;")
                .and_then(|v| v.strip_suffix(b"m"))
                .and_then(number)
            {
                if !self.modify_pending || level > 2 {
                    return false;
                }
                self.modify_pending = false;
                self.modify_supported = true;
                self.modify_entry = level as u8;
                self.modify_level = level as u8;
                if self.inherit && !self.preferences_closed {
                    self.profile.modify_other_keys = Some(level as u8);
                }
                return true;
            }
            if data.starts_with(b"?") && data.ends_with(b"c") && self.da_pending {
                self.da_pending = false;
                return true;
            }
            if let Some(scheme) = data
                .strip_prefix(b"?997;")
                .and_then(|v| v.strip_suffix(b"n"))
                .and_then(number)
            {
                if !self.scheme_pending || !(1..=2).contains(&scheme) {
                    return false;
                }
                self.scheme_pending = false;
                if !self.preferences_closed {
                    self.profile.scheme = Some(if scheme == 2 { 0 } else { 1 });
                }
                return true;
            }
        }
        if let Some(body) = control_string(raw, b']', 0x9d) {
            return self.consume_color(body);
        }
        if let Some(body) = control_string(raw, b'P', 0x90) {
            if !self.cursor_pending || body.len() >= REPLY_LIMIT {
                return false;
            }
            if let Some(style) = body
                .strip_prefix(b"1$r")
                .and_then(|v| v.strip_suffix(b" q"))
                .and_then(number)
                .filter(|v| (1..=6).contains(v))
            {
                self.cursor_pending = false;
                if !self.preferences_closed {
                    self.cursor_entry = Some(style as u8);
                }
                return true;
            }
            if body == b"0$r" || body.starts_with(b"0$r") && body.ends_with(b" q") {
                self.cursor_pending = false;
                return true;
            }
        }
        false
    }

    fn consume_color(&mut self, data: &[u8]) -> bool {
        if data.len() >= REPLY_LIMIT {
            return false;
        }
        let Some((command, values)) = split2(data) else {
            return false;
        };
        let Some(command) = number(command) else {
            return false;
        };
        if command == 4 {
            let fields: Vec<_> = values.split(|b| *b == b';').collect();
            if fields.len() % 2 != 0 {
                return false;
            }
            let mut consumed = false;
            for pair in fields.as_chunks::<2>().0 {
                let Some(index) = number(pair[0]).and_then(|v| u8::try_from(v).ok()) else {
                    continue;
                };
                if !self.palette.contains(&index) {
                    continue;
                }
                if let Some(color) = parse_color(pair[1]) {
                    self.palette.remove(&index);
                    if !self.preferences_closed {
                        self.profile.palette.insert(index, color);
                    }
                    consumed = true;
                }
            }
            return consumed;
        }
        let Ok(command) = u16::try_from(command) else {
            return false;
        };
        if !self.colors.contains(&command) {
            return false;
        }
        let Some(color) = parse_color(values) else {
            return false;
        };
        self.colors.remove(&command);
        if !self.preferences_closed {
            match command {
                10 => self.profile.foreground = Some(color),
                11 => self.profile.background = Some(color),
                12 => self.profile.cursor = Some(color),
                _ => (),
            }
        }
        true
    }

    pub fn child_options(&self, cols: u16, rows: u16) -> Options {
        let mut options = Options::new(cols, rows);
        options.cell_width_px = self.cell_width;
        options.cell_height_px = self.cell_height;
        options.grapheme_width = self.grapheme_width();
        options.terminfo_name = "xterm-256color".into();
        if self.inherit {
            options.profile = self.profile.clone();
            if let Some(n) = self.cursor_entry {
                options.profile.cursor_style = Some([1, 2, 0][usize::from((n - 1) / 2)]);
                options.profile.cursor_blink = Some(n % 2 == 1);
            }
        }
        options.indexed_entries = match self.color_profile {
            ColorProfile::Ansi => 16,
            ColorProfile::Ansi256 => 256,
            _ => 0,
        };
        options
    }

    pub fn enter(&mut self) -> Result<()> {
        self.buffer.extend(b"\x1b[?1049h");
        self.alternate = true;
        self.write_entry_modes();
        self.set_mode(2026, false);
        self.push_kitty();
        self.set_mode(7, true);
        self.set_mode(5, false);
        self.set_mode(2027, self.grapheme_width());
        self.set_mode(2004, true);
        self.flush()
    }
    fn switchable(&self, mode: u16) -> bool {
        matches!(self.entry.get(&mode), Some(1 | 2))
    }
    fn write_mode(&mut self, mode: u16, on: bool) {
        self.buffer
            .extend(format!("\x1b[?{mode}{}", if on { 'h' } else { 'l' }).bytes());
        self.applied.insert(mode, on);
    }
    pub(crate) fn set_mode(&mut self, mode: u16, on: bool) {
        if self.switchable(mode) && self.applied.get(&mode).copied().unwrap_or(false) != on {
            self.write_mode(mode, on);
        }
    }
    fn push_kitty(&mut self) {
        if self.kitty_supported && !self.kitty_pushed {
            self.buffer.extend(b"\x1b[>0u");
            self.kitty_pushed = true;
            self.kitty_flags = 0;
        }
    }
    fn exclusive(&mut self, group: &[u16], active: u16, on: bool) {
        for &mode in group {
            if on && mode == active
                || !self.applied.get(&mode).copied().unwrap_or(false)
                || !self.switchable(mode)
            {
                continue;
            }
            self.set_mode(mode, false);
            if on && self.switchable(active) {
                self.applied.insert(active, false);
            }
        }
        if on {
            self.set_mode(active, true);
        }
    }
    pub fn sync_input(&mut self, state: &InputState) -> Result<()> {
        let measured = self.cell_width > 0 && self.cell_height > 0;
        let localizable = measured || self.entry.get(&1016) != Some(&3);
        for mode in [1, 66, 67, 1035, 1036, 1039, 1004] {
            self.set_mode(mode, state.modes.get(&mode).copied().unwrap_or(false));
        }
        let tracking = state.mouse_tracking_mode != 0;
        let active = match state.mouse_tracking_mode {
            1 => 9,
            2 => 1000,
            3 => 1002,
            4 => 1003,
            _ => 0,
        };
        self.exclusive(TRACKING, active, tracking && localizable);
        let format = if state.modes.get(&1016) == Some(&true) && measured && self.switchable(1016) {
            1016
        } else {
            1006
        };
        self.exclusive(FORMATS, format, tracking);
        self.set_mode(
            1007,
            state.alternate && state.modes.get(&1007) == Some(&true) && !tracking,
        );
        self.push_kitty();
        let flags = if self.kitty_supported {
            state.kitty_flags
        } else {
            0
        };
        if flags != self.kitty_flags {
            self.buffer.extend(format!("\x1b[={flags};1u").bytes());
            self.kitty_flags = flags;
        }
        if self.modify_supported {
            self.set_modify(if state.modify_other_keys_2 { 2 } else { 0 });
        }
        Ok(())
    }
    fn set_modify(&mut self, level: u8) {
        if self.modify_level != level {
            self.buffer.extend(format!("\x1b[>4;{level}m").bytes());
            self.modify_level = level;
        }
    }
    fn write_entry_modes(&mut self) {
        for &mode in MODES {
            if mode == 1049 || !self.switchable(mode) {
                continue;
            }
            let on = self.entry[&mode] == 1;
            if on && (TRACKING.contains(&mode) || FORMATS.contains(&mode)) {
                continue;
            }
            self.write_mode(mode, on);
        }
        for group in [TRACKING, FORMATS] {
            let mut previous = None;
            for &mode in group {
                if !self.switchable(mode) || self.entry[&mode] != 1 {
                    continue;
                }
                self.write_mode(mode, true);
                if let Some(previous) = previous {
                    self.applied.insert(previous, false);
                }
                previous = Some(mode);
            }
        }
    }
    pub fn sync_cursor(&mut self, state: &State) {
        if let Some(entry) = self.cursor_entry {
            let style = match state.cursor.style {
                0 => 6,
                2 => 4,
                _ => 2,
            } - u8::from(state.cursor.blinking);
            let changed = style != entry;
            if self.cursor_style.is_some() != changed || changed && self.cursor_style != Some(style)
            {
                self.buffer
                    .extend(format!("\x1b[{} q", if changed { style } else { 0 }).bytes());
                self.cursor_style = changed.then_some(style);
            }
        }
        if let Some(entry) = self.profile.cursor {
            let color = state.colors.cursor.unwrap_or(entry);
            let changed = color != entry;
            if self.cursor_color.is_some() != changed || changed && self.cursor_color != Some(color)
            {
                if changed {
                    self.buffer.extend(
                        format!("\x1b]12;#{:02x}{:02x}{:02x}\x07", color.0, color.1, color.2)
                            .bytes(),
                    );
                } else {
                    self.buffer.extend(b"\x1b]112\x07");
                }
                self.cursor_color = changed.then_some(color);
            }
        }
    }
    pub fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        if self.kitty_pushed {
            self.buffer.extend(b"\x1b[<1u");
        }
        if self.alternate {
            self.buffer.extend(b"\x1b[?1049l");
        }
        self.write_entry_modes();
        if self.cursor_style.is_some() {
            self.buffer.extend(b"\x1b[0 q");
        }
        if self.cursor_color.is_some() {
            self.buffer.extend(b"\x1b]112\x07");
        }
        if self.modify_supported {
            self.set_modify(self.modify_entry);
        }
        let output = self.flush();
        let settings = transport::set_settings(self.input.as_raw_fd(), self.original_settings());
        OWNERS.get().unwrap().lock().unwrap().remove(&self.device);
        match (output, settings) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) => Err(e),
            (Ok(()), Err(e)) => Err(e.into()),
            (Err(a), Err(b)) => Err(crate::ErrorGroup {
                errors: vec![a, b.into()],
            }
            .into()),
        }
    }
}
impl Drop for Console<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn split2(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = data.iter().position(|b| *b == b';')?;
    Some((&data[..i], &data[i + 1..]))
}
fn number(data: &[u8]) -> Option<u32> {
    if data.is_empty() || !data.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(data).ok()?.parse().ok()
}
fn control_string(raw: &[u8], kind: u8, c1: u8) -> Option<&[u8]> {
    let body = raw
        .strip_prefix(&[0x1b, kind])
        .or_else(|| raw.strip_prefix(&[c1]))?;
    body.strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(b"\x9c"))
        .or_else(|| {
            if kind == b']' {
                body.strip_suffix(b"\x07")
            } else {
                None
            }
        })
}
fn parse_color(bytes: &[u8]) -> Option<Rgb> {
    let mut color = GhosttyColorRgb::default();
    if unsafe { ghostty_color_parse(bytes.as_ptr().cast(), bytes.len(), &mut color) }
        == GHOSTTY_SUCCESS
    {
        Some(Rgb(color.r, color.g, color.b))
    } else {
        None
    }
}
fn detect_color_profile() -> ColorProfile {
    if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        return ColorProfile::None;
    }
    let keys = [
        "TERM",
        "NO_COLOR",
        "COLORTERM",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "WT_SESSION",
        "GOOGLE_CLOUD_SHELL",
        "TMUX",
    ];
    let environment = keys
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect();
    let mut profile = environment_profile(&environment);
    if profile == ColorProfile::None || profile == ColorProfile::TrueColor {
        return profile;
    }
    let term = environment
        .get("TERM")
        .map(String::as_str)
        .unwrap_or_default();
    if !term.is_empty()
        && term != "dumb"
        && terminfo::Database::from_name(term)
            .is_ok_and(|database| database.raw("Tc").is_some() || database.raw("RGB").is_some())
    {
        profile = ColorProfile::TrueColor;
    }
    if environment.get("TMUX").is_some_and(|v| !v.is_empty()) {
        profile = profile.max(ColorProfile::Ansi256);
        if bounded_truecolor_probe(
            std::process::Command::new("tmux").arg("info"),
            Duration::from_millis(300),
        )
        .unwrap_or(false)
        {
            profile = ColorProfile::TrueColor;
        }
    }
    profile
}

fn bounded_truecolor_probe(
    command: &mut std::process::Command,
    timeout: Duration,
) -> io::Result<bool> {
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let outcome = (|| {
        let stdout = child.stdout.as_ref().expect("piped probe output");
        let fd = stdout.as_raw_fd();
        let flags = transport::cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
        transport::cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            // Reap before draining: a completed process cannot append another
            // capability line between the final read and the status check.
            let status = child.try_wait()?;
            loop {
                match transport::read(fd, &mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if output.len() + count > 64 << 10 {
                            return Ok(false);
                        }
                        output.extend_from_slice(&buffer[..count]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            if let Some(status) = status {
                return Ok(status.success()
                    && output.split(|b| *b == b'\n').any(|line| {
                        (line.windows(2).any(|w| w == b"Tc")
                            || line.windows(3).any(|w| w == b"RGB"))
                            && line.windows(4).any(|w| w == b"true")
                    }));
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            let mut fds = [libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            }];
            transport::poll(
                &mut fds,
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .saturating_add(1)
                    .min(20) as i32,
            )?;
        }
    })();
    let _ = child.kill();
    let waited = child.wait();
    match (outcome, waited) {
        (Err(error), _) => Err(error),
        (Ok(value), Ok(_)) => Ok(value),
        (Ok(_), Err(error)) => Err(error),
    }
}
fn environment_profile(environment: &BTreeMap<String, String>) -> ColorProfile {
    let value = |key: &str| environment.get(key).map(String::as_str).unwrap_or_default();
    if !value("NO_COLOR").is_empty() {
        return ColorProfile::None;
    }
    let term = value("TERM");
    let forced = matches!(
        value("CLICOLOR_FORCE"),
        "1" | "t" | "T" | "true" | "TRUE" | "True"
    );
    if (term.is_empty() || term == "dumb") && !forced {
        return ColorProfile::None;
    }
    let color = value("COLORTERM").to_ascii_lowercase();
    if [
        "alacritty",
        "contour",
        "foot",
        "ghostty",
        "kitty",
        "rio",
        "st",
        "wezterm",
    ]
    .iter()
    .any(|name| term.contains(name))
        || !value("WT_SESSION").is_empty()
        || matches!(
            value("GOOGLE_CLOUD_SHELL"),
            "1" | "true" | "TRUE" | "True" | "t" | "T"
        )
        || matches!(color.as_str(), "truecolor" | "24bit" | "yes" | "true")
            && !term.starts_with("screen")
            && !term.starts_with("tmux")
        || term.ends_with("direct")
    {
        return ColorProfile::TrueColor;
    }
    if term.starts_with("screen") || term.starts_with("tmux") || term.ends_with("256color") {
        ColorProfile::Ansi256
    } else {
        ColorProfile::Ansi
    }
}

#[cfg(test)]
#[path = "../tests/unit/console.rs"]
mod tests;
