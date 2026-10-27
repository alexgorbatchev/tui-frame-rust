//! Lossless input framing. Read boundaries never define protocol boundaries.
use std::collections::VecDeque;
use termina::event::KeyEvent;
pub use termina::event::{KeyCode, KeyEventKind, Modifiers};
mod decoder;
use decoder::{decode, decode_deadline};

const LIMIT: usize = 1 << 20;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub raw: Vec<u8>,
    pub event: KeyEvent,
    pub codepoint: Option<u32>,
    pub base_code: Option<u32>,
    pub shifted_code: Option<u32>,
    pub virtual_key: Option<u32>,
    pub win32: Option<Win32>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Win32 {
    pub scan_code: u16,
    pub unicode: u16,
    pub control_state: u32,
    pub repeat_count: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
    Wheel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub action: MouseAction,
    /// Ghostty's native button identity: left=1, right=2, middle=3.
    pub button: Option<u32>,
    pub modifiers: Modifiers,
    pub x: i64,
    pub y: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Mouse(Mouse),
    PasteStart,
    PasteEnd,
    Focus(bool),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub raw: Vec<u8>,
    pub event: Event,
    pub paste: bool,
}

/// Completed packets remain available even when the following sequence is oversized.
#[derive(Debug, thiserror::Error)]
#[error("terminal input sequence exceeds framing limit")]
pub struct FrameError {
    pub packets: Vec<Packet>,
}

#[derive(Default)]
pub struct Framer {
    buffer: VecDeque<u8>,
    paste: bool,
    win32_state: u32,
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn pending(&self) -> bool {
        !self.buffer.is_empty()
    }
    pub fn needs_deadline(&self) -> bool {
        !self.paste
            && self.buffer.front() == Some(&0x1b)
            && (self.buffer.len() == 1
                || (self.buffer.len() == 2
                    && self
                        .buffer
                        .get(1)
                        .is_some_and(|b| (0x20..=0x7e).contains(b))))
    }
    pub fn feed(&mut self, raw: &[u8]) -> Result<Vec<Packet>, FrameError> {
        self.buffer.extend(raw);
        let mut packets = Vec::new();
        while !self.buffer.is_empty() {
            if self.paste {
                let n = if let Some(i) = self
                    .buffer
                    .make_contiguous()
                    .windows(PASTE_END.len())
                    .position(|s| s == PASTE_END)
                {
                    if i == 0 {
                        let mut p = self.take(PASTE_END.len());
                        p.event = Event::PasteEnd;
                        self.paste = false;
                        packets.push(p);
                        continue;
                    }
                    i
                } else {
                    let buffer = self.buffer.make_contiguous();
                    let keep = (1..PASTE_END.len())
                        .filter(|&n| buffer.ends_with(&PASTE_END[..n]))
                        .max()
                        .unwrap_or(0);
                    self.buffer.len() - keep
                };
                if n == 0 {
                    break;
                }
                let mut p = self.take(n);
                p.paste = true;
                packets.push(p);
                continue;
            }
            let n = complete_length(self.buffer.make_contiguous());
            if n > LIMIT || (n == 0 && self.buffer.len() > LIMIT) {
                return Err(FrameError { packets });
            }
            if n == 0 {
                break;
            }
            let mut p = self.take(n);
            if p.raw == b"\x1b[200~" {
                self.paste = true;
                p.event = Event::PasteStart;
            } else {
                p.event = decode(&p.raw, &mut self.win32_state);
            }
            packets.push(p);
        }
        Ok(packets)
    }
    pub fn flush(&mut self) -> Option<Packet> {
        if self.buffer.is_empty() {
            return None;
        }
        let deadline = self.needs_deadline();
        let mut p = self.take(self.buffer.len());
        p.paste = self.paste;
        if deadline {
            p.event = decode_deadline(&p.raw);
        }
        Some(p)
    }
    fn take(&mut self, n: usize) -> Packet {
        Packet {
            raw: self.buffer.drain(..n).collect(),
            event: Event::Unknown,
            paste: false,
        }
    }
}

fn rune_length(raw: &[u8]) -> usize {
    let n = match raw[0] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    if raw.len() < n {
        // A known malformed prefix is a complete replacement byte, not a pending rune.
        if raw[1..].iter().any(|b| b & 0xc0 != 0x80) {
            1
        } else {
            0
        }
    } else if std::str::from_utf8(&raw[..n]).is_ok() {
        n
    } else {
        1
    }
}

fn complete_length(raw: &[u8]) -> usize {
    let (prefix, control) = match raw {
        [0x1b] => return 0,
        [0x1b, b'O', ..] => (2, b'O'),
        [0x1b, c @ (b'[' | b']' | b'P' | b'_' | b'^' | b'X'), ..] => (2, *c),
        [0x1b, rest @ ..] => {
            let n = rune_length(rest);
            return if n == 0 { 0 } else { n + 1 };
        }
        [0x8f, ..] => (1, b'O'),
        [0x9b, ..] => (1, b'['),
        [0x9d, ..] => (1, b']'),
        [0x90, ..] => (1, b'P'),
        [0x9f, ..] => (1, b'_'),
        [0x9e, ..] => (1, b'^'),
        [0x98, ..] => (1, b'X'),
        _ => return rune_length(raw),
    };
    if control == b'O' {
        return raw[prefix..]
            .iter()
            .position(|b| !b.is_ascii_digit())
            .map_or(0, |i| prefix + i + 1);
    }
    if control == b'[' {
        if raw.get(prefix) == Some(&b'M') {
            return if raw.len() >= prefix + 4 {
                prefix + 4
            } else {
                0
            };
        }
        for (i, &b) in raw.iter().enumerate().skip(prefix) {
            if (0x40..=0x7e).contains(&b) || matches!(b, 0x18 | 0x1a) {
                return i + 1;
            }
            if b == 0x1b {
                return i;
            }
        }
        return 0;
    }
    for (i, &b) in raw.iter().enumerate().skip(prefix) {
        if b == 0x9c || matches!(b, 0x18 | 0x1a) || (control == b']' && b == 7) {
            return i + 1;
        }
        if b == 0x1b {
            return match raw.get(i + 1) {
                Some(b'\\') => i + 2,
                Some(_) => i,
                None => 0,
            };
        }
    }
    0
}
