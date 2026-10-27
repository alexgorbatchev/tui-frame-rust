//! Capture ownership and native child-protocol mouse encoding.
use crate::{
    Error, Result, ffi,
    input::{Event, Key, KeyCode, KeyEventKind, Modifiers, Mouse, MouseAction, Packet},
    terminal::{InputState, Terminal},
};
use ratatui::layout::Rect;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
    Pass,
    Consume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    User,
    Captured,
    Paste,
    Focus,
    Mouse,
    OutsideChild,
    UnmeasuredPixels,
    UnownedGesture,
    MouseDisabled,
    Unroutable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub bytes: Vec<u8>,
    pub origin: Origin,
    pub captured: bool,
    pub error: Option<String>,
}
impl Route {
    fn new(origin: Origin, bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            origin,
            captured: false,
            error: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HostProfile {
    pub kitty_flags: u8,
    pub mouse_pixels: bool,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

struct Gesture {
    code: KeyCode,
    codepoint: Option<u32>,
    base: Option<u32>,
    disposition: Capture,
}
impl Gesture {
    fn continues(&self, key: &Key) -> bool {
        match (
            self.base.filter(|v| *v != 0),
            key.base_code.filter(|v| *v != 0),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => match (self.codepoint, key.codepoint) {
                (Some(a), Some(b)) => a == b,
                _ => self.code == key.event.code,
            },
        }
    }
}

#[derive(Default)]
pub struct Router {
    pub host: HostProfile,
    releases: bool,
    gestures: Vec<Gesture>,
    mouse_owners: BTreeMap<Option<u32>, bool>,
    paste_markers: Option<bool>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }
    /// Bound native expansion, including an anonymous release of all five held buttons.
    pub fn max_output_size(packet: &Packet) -> usize {
        packet
            .raw
            .len()
            .saturating_mul(if matches!(packet.event, Event::Mouse(_)) {
                12
            } else {
                1
            })
    }
    pub fn release_gestures(&mut self) {
        self.gestures.clear();
    }
    pub fn route(
        &mut self,
        packet: &Packet,
        terminal: &mut Terminal,
        viewport: Rect,
        capture: Option<&mut dyn FnMut(&Key) -> Capture>,
    ) -> Result<Route> {
        if packet.paste {
            return Ok(Route::new(Origin::User, packet.raw.clone()));
        }
        match &packet.event {
            Event::Key(key) => {
                let releases = self.host.kitty_flags & 2 != 0;
                if self.releases != releases {
                    self.release_gestures();
                    self.releases = releases;
                }
                let disposition = self.capture(key, capture);
                let mut route = Route::new(
                    if disposition == Capture::Consume {
                        Origin::Captured
                    } else {
                        Origin::User
                    },
                    Vec::new(),
                );
                route.captured = disposition == Capture::Consume;
                if !route.captured {
                    route.bytes = packet.raw.clone();
                }
                Ok(route)
            }
            Event::PasteStart => {
                let enabled = mode(&terminal.input_state()?, 2004)?;
                self.paste_markers = Some(enabled);
                Ok(Route::new(
                    Origin::Paste,
                    if enabled {
                        packet.raw.clone()
                    } else {
                        Vec::new()
                    },
                ))
            }
            Event::PasteEnd => Ok(Route::new(
                Origin::Paste,
                if self.paste_markers.take() == Some(true) {
                    packet.raw.clone()
                } else {
                    Vec::new()
                },
            )),
            Event::Focus(_) => Ok(Route::new(
                Origin::Focus,
                if mode(&terminal.input_state()?, 1004)? {
                    packet.raw.clone()
                } else {
                    Vec::new()
                },
            )),
            Event::Mouse(mouse) => {
                let state = terminal.input_state()?;
                match self.mouse(*mouse, terminal, &state, viewport) {
                    Err(Error::Unroutable(reason)) => {
                        let mut route = Route::new(Origin::Unroutable, Vec::new());
                        route.error = Some(reason.into());
                        Ok(route)
                    }
                    result => result,
                }
            }
            Event::Unknown => Ok(Route::new(Origin::User, packet.raw.clone())),
        }
    }

    fn capture(&mut self, key: &Key, handler: Option<&mut dyn FnMut(&Key) -> Capture>) -> Capture {
        let Some(handler) = handler else {
            return Capture::Pass;
        };
        let mut disposition = handler(key);
        if !self.releases {
            return disposition;
        }
        let held = self.gestures.iter().position(|g| g.continues(key));
        match key.event.kind {
            KeyEventKind::Release => {
                if let Some(i) = held {
                    disposition = self.gestures.remove(i).disposition;
                }
            }
            KeyEventKind::Repeat if held.is_some() => {
                disposition = self.gestures[held.unwrap()].disposition;
            }
            _ => {
                let gesture = Gesture {
                    code: key.event.code,
                    codepoint: key.codepoint,
                    base: key.base_code,
                    disposition,
                };
                if let Some(i) = held {
                    self.gestures[i] = gesture;
                } else {
                    self.gestures.push(gesture);
                }
            }
        }
        disposition
    }

    fn mouse(
        &mut self,
        event: Mouse,
        terminal: &mut Terminal,
        state: &InputState,
        viewport: Rect,
    ) -> Result<Route> {
        let (x, y, inside) = self.position(event, state, viewport)?;
        let press = event.action == MouseAction::Press;
        let release = event.action == MouseAction::Release;
        let mut owned = self
            .mouse_owners
            .get(&event.button)
            .copied()
            .unwrap_or(false);
        let anonymous = release && event.button.is_none();
        let mut released = Vec::new();
        if anonymous {
            // Stable native semantic order: left, middle, right, back, forward.
            for button in [1, 3, 2, 8, 9] {
                if self.mouse_owners.get(&Some(button)) == Some(&true) {
                    released.push(Some(button));
                }
            }
            owned |= !released.is_empty();
            self.mouse_owners.clear();
            if released.is_empty() && owned {
                released.push(None);
            }
        } else if release {
            self.mouse_owners.remove(&event.button);
        }
        if !inside {
            if press {
                self.mouse_owners.insert(event.button, false);
            }
            return Ok(Route::new(
                if self.unmeasured_pixels() {
                    Origin::UnmeasuredPixels
                } else {
                    Origin::OutsideChild
                },
                Vec::new(),
            ));
        }
        if (release || (event.action == MouseAction::Motion && event.button.is_some())) && !owned {
            return Ok(Route::new(Origin::UnownedGesture, Vec::new()));
        }
        if state.mouse_tracking_mode == ffi::GHOSTTY_MOUSE_TRACKING_NONE {
            return Ok(Route::new(Origin::MouseDisabled, Vec::new()));
        }
        wire_bounds(x, y, state)?;
        if press {
            self.mouse_owners.insert(event.button, true);
        }
        let mut bytes = Vec::new();
        let buttons = if anonymous {
            released
        } else {
            vec![event.button]
        };
        for button in buttons {
            let encoded = self.encode(Mouse { button, ..event }, x, y, terminal, state);
            match encoded {
                Ok(output) => bytes.extend(output),
                Err(error) => {
                    if press {
                        self.mouse_owners.insert(event.button, false);
                    }
                    return Err(error);
                }
            }
        }
        if press && bytes.is_empty() {
            self.mouse_owners.insert(event.button, false);
        }
        Ok(Route::new(Origin::Mouse, bytes))
    }

    fn encode(
        &self,
        event: Mouse,
        x: f32,
        y: f32,
        terminal: &mut Terminal,
        state: &InputState,
    ) -> Result<Vec<u8>> {
        let modifiers = native_modifiers(event.modifiers)?;
        let action = match event.action {
            MouseAction::Press | MouseAction::Wheel => 0,
            MouseAction::Release => 1,
            MouseAction::Motion => 2,
        };
        let bytes = terminal.encode_mouse(
            crate::terminal::Mouse {
                action,
                button: event.button,
                modifiers,
                x,
                y,
            },
            self.mouse_owners.values().any(|v| *v),
        )?;
        if std::str::from_utf8(&bytes).is_err() && mode(state, 1005)? {
            return Err(Error::Unroutable(
                "native UTF-8 mouse encoder cannot represent this button",
            ));
        }
        Ok(bytes)
    }

    fn unmeasured_pixels(&self) -> bool {
        self.host.mouse_pixels && (self.host.cell_width_px == 0 || self.host.cell_height_px == 0)
    }

    fn position(
        &self,
        event: Mouse,
        state: &InputState,
        viewport: Rect,
    ) -> Result<(f32, f32, bool)> {
        let pixels = mode(state, 1016)?;
        if viewport.is_empty() {
            return Err(Error::Session("mouse routing has no child viewport".into()));
        }
        if self.unmeasured_pixels() {
            return Ok((0.0, 0.0, false));
        }
        if self.host.mouse_pixels {
            if self.host.cell_width_px != state.cell_width_px
                || self.host.cell_height_px != state.cell_height_px
            {
                return Err(Error::Session(
                    "pixel mouse input requires matching measured cell geometry".into(),
                ));
            }
            let (w, h) = (
                i128::from(self.host.cell_width_px),
                i128::from(self.host.cell_height_px),
            );
            let (x, y) = (
                i128::from(event.x) - i128::from(viewport.x) * w,
                i128::from(event.y) - i128::from(viewport.y) * h,
            );
            let inside = x >= 0
                && y >= 0
                && x < i128::from(viewport.width) * w
                && y < i128::from(viewport.height) * h;
            return Ok((
                (x + i128::from(pixels)) as f32,
                (y + i128::from(pixels)) as f32,
                inside,
            ));
        }
        let (x, y) = (
            i128::from(event.x) - i128::from(viewport.x),
            i128::from(event.y) - i128::from(viewport.y),
        );
        let inside =
            x >= 0 && y >= 0 && x < i128::from(viewport.width) && y < i128::from(viewport.height);
        if !inside {
            return Ok((0.0, 0.0, false));
        }
        if pixels {
            return Err(Error::Unroutable(
                "child pixel mouse requires pixel input from outer terminal",
            ));
        }
        let (w, h) = cell_size(state);
        Ok((x as f32 * w, y as f32 * h, true))
    }
}

fn cell_size(state: &InputState) -> (f32, f32) {
    if state.cell_width_px == 0 || state.cell_height_px == 0 {
        (1.0, 1.0)
    } else {
        (state.cell_width_px as f32, state.cell_height_px as f32)
    }
}

fn mode(state: &InputState, number: u16) -> Result<bool> {
    if let Some(error) = state.mode_errors.get(&number) {
        return Err(Error::Session(format!(
            "read child mode {number} for routing: {error}"
        )));
    }
    state
        .modes
        .get(&number)
        .copied()
        .ok_or_else(|| Error::Session(format!("child mode {number} is unavailable for routing")))
}

fn wire_bounds(x: f32, y: f32, state: &InputState) -> Result<()> {
    for number in [1016, 1006, 1015] {
        if mode(state, number)? {
            return Ok(());
        }
    }
    let limit = if mode(state, 1005)? { 2015.0 } else { 223.0 };
    let (w, h) = cell_size(state);
    if x / w >= limit || y / h >= limit {
        return Err(Error::Unroutable(
            "child mouse encoding cannot represent coordinates",
        ));
    }
    Ok(())
}

fn native_modifiers(modifiers: Modifiers) -> Result<u16> {
    if modifiers.intersects(Modifiers::HYPER | Modifiers::META) {
        return Err(Error::Unroutable("unsupported native mouse modifier"));
    }
    let mut native = 0;
    for (modifier, bit) in [
        (Modifiers::SHIFT, ffi::GHOSTTY_MODS_SHIFT),
        (Modifiers::ALT, ffi::GHOSTTY_MODS_ALT),
        (Modifiers::CONTROL, ffi::GHOSTTY_MODS_CTRL),
        (Modifiers::SUPER, ffi::GHOSTTY_MODS_SUPER),
        (Modifiers::CAPS_LOCK, ffi::GHOSTTY_MODS_CAPS_LOCK),
        (Modifiers::NUM_LOCK, ffi::GHOSTTY_MODS_NUM_LOCK),
    ] {
        if modifiers.contains(modifier) {
            native |= bit as u16;
        }
    }
    Ok(native)
}
