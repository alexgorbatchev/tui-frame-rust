use super::{Event, Key, KeyCode, KeyEventKind, Modifiers, Mouse, MouseAction, Win32};
use termina::{
    Parser,
    event::{KeyEvent, KeyEventState},
};

fn native_key(raw: &[u8]) -> Option<KeyEvent> {
    let mut parser = Parser::default();
    parser.parse(raw, false);
    let termina::Event::Key(key) = parser.pop()? else {
        return None;
    };
    if parser.pop().is_some() {
        None
    } else {
        Some(key)
    }
}

fn key(raw: &[u8], event: KeyEvent) -> Key {
    Key {
        raw: raw.to_vec(),
        codepoint: match event.code {
            KeyCode::Char(c) => Some(u32::from(c.to_ascii_lowercase())),
            _ => None,
        },
        event,
        base_code: None,
        shifted_code: None,
        virtual_key: None,
        win32: None,
        text: String::new(),
    }
}

pub(super) fn decode_deadline(raw: &[u8]) -> Event {
    if raw == b"\x1b" {
        return Event::Key(key(raw, KeyEvent::new(KeyCode::Escape, Modifiers::NONE)));
    }
    if let [0x1b, c] = raw
        && let Some(mut event) = native_key(&[*c])
    {
        event.modifiers |= Modifiers::ALT;
        return Event::Key(key(raw, event));
    }
    Event::Unknown
}

pub(super) fn decode(raw: &[u8], win32_state: &mut u32) -> Event {
    let normalized;
    let bytes = match raw.first() {
        Some(0x8f) => {
            normalized = [b"\x1bO".as_slice(), &raw[1..]].concat();
            normalized.as_slice()
        }
        Some(0x9b) => {
            normalized = [b"\x1b[".as_slice(), &raw[1..]].concat();
            normalized.as_slice()
        }
        _ => raw,
    };
    if bytes == b"\x1b[I" {
        return Event::Focus(true);
    }
    if bytes == b"\x1b[O" {
        return Event::Focus(false);
    }
    if let Some(mouse) = decode_mouse(bytes) {
        return Event::Mouse(mouse);
    }
    if bytes.starts_with(b"\x1b[") && bytes.ends_with(b"_") {
        return win32_key(raw, bytes, win32_state).map_or(Event::Unknown, Event::Key);
    }
    let mut decoded = native_key(bytes);
    // xterm modifyOtherKeys uses a different envelope for the same native key semantics.
    if bytes.starts_with(b"\x1b[27;")
        && bytes.ends_with(b"~")
        && let Ok(params) = std::str::from_utf8(&bytes[5..bytes.len() - 1])
        && let Some((modifiers, codepoint)) = params.split_once(';')
    {
        let wire = format!("\x1b[{codepoint};{modifiers}u");
        decoded = native_key(wire.as_bytes());
    }
    let mut ss3_code = None;
    if bytes.starts_with(b"\x1bO") {
        let tail = &bytes[2..];
        if let Some((&final_byte, modifiers)) = tail.split_last() {
            let modifier = std::str::from_utf8(modifiers)
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(1)
                .max(1);
            ss3_code = match final_byte {
                b'M' => Some(57414),
                b'X' => Some(57415),
                b'j' => Some(57411),
                b'k' => Some(57413),
                b'l' => Some(57416),
                b'm' => Some(57412),
                b'n' => Some(57409),
                b'o' => Some(57410),
                b'p'..=b'y' => Some(57399 + u32::from(final_byte - b'p')),
                _ => None,
            };
            if let Some(cp) = ss3_code {
                decoded = native_key(format!("\x1b[{cp};{modifier}u").as_bytes());
            } else if (b'a'..=b'd').contains(&final_byte) {
                decoded = native_key(&[0x1b, b'O', final_byte - 32]);
                if let Some(ref mut event) = decoded {
                    event.modifiers |= Modifiers::CONTROL;
                }
            } else if final_byte == b'E' {
                decoded = Some(KeyEvent::new(
                    KeyCode::KeypadBegin,
                    Modifiers::from_bits_retain((modifier - 1) as u8),
                ));
            } else if !modifiers.is_empty() {
                decoded = native_key(&[0x1b, b'O', final_byte]);
            }
            if let Some(ref mut event) = decoded {
                event.modifiers |= Modifiers::from_bits_retain((modifier - 1) as u8);
            }
        }
    }
    let Some(event) = decoded else {
        return Event::Unknown;
    };
    let mut k = key(raw, event);
    if ss3_code.is_some() {
        k.codepoint = ss3_code;
    }
    if let KeyCode::Char(c) = event.code
        && !event.modifiers.intersects(
            Modifiers::ALT
                | Modifiers::CONTROL
                | Modifiers::SUPER
                | Modifiers::HYPER
                | Modifiers::META,
        )
    {
        k.text.push(c);
    }
    if bytes.starts_with(b"\x1b[") && bytes.ends_with(b"u") {
        k.text.clear();
        let Ok(params) = std::str::from_utf8(&bytes[2..bytes.len() - 1]) else {
            return Event::Unknown;
        };
        let fields: Vec<_> = params.split(';').collect();
        let codes: Vec<_> = fields[0].split(':').collect();
        k.codepoint = codes[0].parse().ok();
        k.shifted_code = codes.get(1).and_then(|s| s.parse().ok());
        k.base_code = codes.get(2).and_then(|s| s.parse().ok());
        if let Some(modifiers) = fields
            .get(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.parse::<u16>().ok())
            && let Some(bits) = modifiers.checked_sub(1).and_then(|n| u8::try_from(n).ok())
        {
            k.event.modifiers = Modifiers::from_bits_retain(bits);
        }
        if let Some(text) = fields.get(2) {
            for cp in text.split(':') {
                let Some(c) = cp.parse().ok().and_then(char::from_u32) else {
                    return Event::Unknown;
                };
                k.text.push(c);
            }
        }
    }
    if k.text.is_empty()
        && !k.event.modifiers.intersects(
            Modifiers::ALT
                | Modifiers::CONTROL
                | Modifiers::SUPER
                | Modifiers::HYPER
                | Modifiers::META,
        )
    {
        let keypad = match k.codepoint {
            Some(cp @ 57399..=57408) => char::from_u32(u32::from(b'0') + cp - 57399),
            Some(57409) => Some('.'),
            Some(57410) => Some('/'),
            Some(57411) => Some('*'),
            Some(57412) => Some('-'),
            Some(57413) => Some('+'),
            Some(57415) => Some('='),
            Some(57416) => Some(','),
            _ => None,
        };
        if let Some(c) = keypad {
            k.text.push(c);
        } else if let KeyCode::Char(c) = k.event.code {
            if k.event
                .modifiers
                .intersects(Modifiers::SHIFT | Modifiers::CAPS_LOCK)
            {
                if let Some(shifted) = k.shifted_code.and_then(char::from_u32) {
                    k.text.push(shifted);
                } else {
                    k.text.extend(c.to_uppercase());
                }
            } else {
                k.text.push(c);
            }
        }
    }
    Event::Key(k)
}

fn win32_key(raw: &[u8], bytes: &[u8], previous_state: &mut u32) -> Option<Key> {
    let text = std::str::from_utf8(&bytes[2..bytes.len() - 1]).ok()?;
    let fields: Vec<_> = text.split(';').collect();
    if fields.len() > 6 {
        return None;
    }
    let mut params = [0_u32, 0, 0, 0, 0, 1];
    for (i, s) in fields.iter().enumerate() {
        if !s.is_empty() {
            params[i] = s.parse().ok()?;
        }
    }
    let [vk, scan, cp, down, cs, count] = params;
    let record = Win32 {
        scan_code: u16::try_from(scan).ok()?,
        unicode: u16::try_from(cp).ok()?,
        control_state: cs,
        repeat_count: u16::try_from(count).ok()?.max(1),
    };
    if down > 1 {
        return None;
    }
    let mut modifiers = Modifiers::NONE;
    for (mask, modifier) in [
        (0x10, Modifiers::SHIFT),
        (0x03, Modifiers::ALT),
        (0x0c, Modifiers::CONTROL),
        (0x80, Modifiers::CAPS_LOCK),
        (0x20, Modifiers::NUM_LOCK),
    ] {
        if cs & mask != 0 {
            modifiers |= modifier;
        }
    }
    let code = if cp >= 32 && cp != 127 {
        KeyCode::Char(char::from_u32(cp)?)
    } else {
        match vk {
            8 => KeyCode::Backspace,
            9 => KeyCode::Tab,
            13 => KeyCode::Enter,
            27 => KeyCode::Escape,
            33 => KeyCode::PageUp,
            34 => KeyCode::PageDown,
            35 => KeyCode::End,
            36 => KeyCode::Home,
            37 => KeyCode::Left,
            38 => KeyCode::Up,
            39 => KeyCode::Right,
            40 => KeyCode::Down,
            45 => KeyCode::Insert,
            46 => KeyCode::Delete,
            112..=135 => KeyCode::Function((vk - 111) as u8),
            65..=90 => KeyCode::Char(char::from_u32(vk + 32)?),
            48..=57 => KeyCode::Char(char::from_u32(vk)?),
            _ => win32_special(vk, scan, cs, *previous_state)?.code,
        }
    };
    let mut event = KeyEvent::new(code, modifiers);
    event.kind = if down == 0 {
        KeyEventKind::Release
    } else if count > 1 {
        KeyEventKind::Repeat
    } else {
        KeyEventKind::Press
    };
    let mut k = key(raw, event);
    if (96..=111).contains(&vk) {
        k.event.state |= KeyEventState::KEYPAD;
    }
    k.virtual_key = Some(vk);
    k.codepoint = Some(cp);
    k.base_code = Some(vk);
    k.win32 = Some(record);
    if vk != 0 {
        *previous_state = cs;
    }
    if down != 0 && cp >= 32 && cp != 127 && (cs & !(0x10 | 0x20 | 0x40 | 0x80) == 0 || cs & 9 == 9)
    {
        k.text.push(char::from_u32(cp)?);
    }
    Some(k)
}

fn win32_special(vk: u32, scan: u32, cs: u32, previous: u32) -> Option<KeyEvent> {
    let flags = if cs & 0x1f == 0 { previous } else { cs };
    let cp = match vk {
        16 => {
            if scan == 54 || flags & 0x100 != 0 {
                57447
            } else {
                57441
            }
        }
        17 => {
            if flags & 4 != 0 {
                57448
            } else {
                57442
            }
        }
        18 => {
            if flags & 1 != 0 {
                57449
            } else {
                57443
            }
        }
        19 => 57362,
        20 => 57358,
        44 => 57361,
        91 => 57444,
        92 => 57450,
        93 => 57363,
        96..=105 => 57399 + vk - 96,
        106 => 57411,
        107 => 57413,
        108 => 57416,
        109 => 57412,
        110 => 57409,
        111 => 57410,
        144 => 57360,
        145 => 57359,
        160 => 57441,
        161 => 57447,
        162 => 57442,
        163 => 57448,
        164 => 57443,
        165 => 57449,
        173 => 57440,
        174 => 57438,
        175 => 57439,
        176 => 57435,
        177 => 57436,
        178 => 57432,
        179 => 57430,
        32 => 32,
        186 => 59,
        187 => 61,
        188 => 44,
        189 => 45,
        190 => 46,
        191 => 47,
        192 => 96,
        219 => 91,
        220 => 92,
        221 => 93,
        222 => 39,
        _ => return None,
    };
    native_key(format!("\x1b[{cp}u").as_bytes())
}

fn decode_mouse(raw: &[u8]) -> Option<Mouse> {
    let (cb, x, y, release, legacy) = if raw.starts_with(b"\x1b[M") && raw.len() == 6 {
        (
            u32::from(raw[3]).checked_sub(32)?,
            i64::from(raw[4]) - 33,
            i64::from(raw[5]) - 33,
            false,
            true,
        )
    } else {
        let tail = raw.strip_prefix(b"\x1b[")?;
        let (&final_byte, body) = tail.split_last()?;
        if !matches!(final_byte, b'M' | b'm') {
            return None;
        }
        let sgr = body.starts_with(b"<");
        let body = if sgr { &body[1..] } else { body };
        let fields: Vec<_> = std::str::from_utf8(body).ok()?.split(';').collect();
        if fields.len() != 3 {
            return None;
        }
        let mut cb: u32 = fields[0].parse().ok()?;
        if !sgr {
            cb = cb.checked_sub(32)?;
        }
        (
            cb,
            fields[1].parse::<i64>().ok()?.checked_sub(1)?,
            fields[2].parse::<i64>().ok()?.checked_sub(1)?,
            final_byte == b'm',
            !sgr,
        )
    };
    let mut modifiers = Modifiers::NONE;
    for (mask, modifier) in [
        (4, Modifiers::SHIFT),
        (8, Modifiers::ALT),
        (16, Modifiers::CONTROL),
    ] {
        if cb & mask != 0 {
            modifiers |= modifier;
        }
    }
    let id = (cb & 3)
        + if cb & 128 != 0 {
            128
        } else if cb & 64 != 0 {
            64
        } else {
            0
        };
    let button = match id {
        0 => Some(1),
        1 => Some(3),
        2 => Some(2),
        3 => None,
        64..=67 => Some(id - 60),
        128..=131 => Some(id - 120),
        _ => return None,
    };
    let action = if release || (legacy && id == 3 && cb & 32 == 0) {
        MouseAction::Release
    } else if id & 64 != 0 {
        MouseAction::Wheel
    } else if cb & 32 != 0 {
        MouseAction::Motion
    } else {
        MouseAction::Press
    };
    Some(Mouse {
        action,
        button,
        modifiers,
        x,
        y,
    })
}
