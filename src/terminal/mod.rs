//! Single-owner libghostty terminal, owned observations, and ordered effects.

mod callbacks;
mod encoding;
mod snapshot;
mod types;
pub use types::*;

use crate::{Error, Result, ffi::*};
use std::{collections::BTreeMap, ffi::c_void, marker::PhantomData, ptr, rc::Rc};

pub(crate) fn check(code: GhosttyResult, operation: &'static str) -> Result<()> {
    if code == GHOSTTY_SUCCESS {
        Ok(())
    } else {
        Err(Error::Native { operation, code })
    }
}

/// Every native operation is serialized by this owner. A terminal is neither
/// `Send` nor `Sync`; its callbacks run on the calling thread.
pub struct Terminal {
    core: Box<Core>,
    _single_thread: PhantomData<Rc<()>>,
}

struct Core {
    native: GhosttyTerminal,
    render: GhosttyRenderState,
    rows: GhosttyRenderStateRowIterator,
    cells: GhosttyRenderStateRowCells,
    keys: GhosttyKeyEncoder,
    key_probe: GhosttyKeyEvent,
    mouse: GhosttyMouseEncoder,
    mouse_event: GhosttyMouseEvent,
    options: Options,
    effects: Vec<Effect>,
    callback_error: Option<Error>,
    held: bool,
    hold_pending: bool,
    visual: Option<Visual>,
    visual_rows: Vec<snapshot::VisualRow>,
    dirty: Vec<bool>,
}

impl Drop for Core {
    fn drop(&mut self) {
        // Native free functions explicitly accept null handles. Iterator and
        // encoder lifetimes end before the terminal they observe.
        unsafe {
            ghostty_mouse_event_free(self.mouse_event);
            ghostty_mouse_encoder_free(self.mouse);
            ghostty_key_event_free(self.key_probe);
            ghostty_key_encoder_free(self.keys);
            ghostty_render_state_row_cells_free(self.cells);
            ghostty_render_state_row_iterator_free(self.rows);
            ghostty_render_state_free(self.render);
            ghostty_terminal_free(self.native);
        }
    }
}

impl Terminal {
    pub fn new(options: Options) -> Result<Self> {
        valid_size(options.cols, options.rows)?;
        let mut core = Box::new(Core {
            native: ptr::null_mut(),
            render: ptr::null_mut(),
            rows: ptr::null_mut(),
            cells: ptr::null_mut(),
            keys: ptr::null_mut(),
            key_probe: ptr::null_mut(),
            mouse: ptr::null_mut(),
            mouse_event: ptr::null_mut(),
            options,
            effects: Vec::new(),
            callback_error: None,
            held: false,
            hold_pending: false,
            visual: None,
            visual_rows: Vec::new(),
            dirty: Vec::new(),
        });
        unsafe {
            check(
                ghostty_terminal_new(
                    ptr::null(),
                    &mut core.native,
                    core.options.cols,
                    core.options.rows,
                ),
                "create terminal",
            )?;
            check(
                ghostty_render_state_new(ptr::null(), &mut core.render),
                "create render state",
            )?;
            check(
                ghostty_render_state_row_iterator_new(ptr::null(), &mut core.rows),
                "create row iterator",
            )?;
            check(
                ghostty_render_state_row_cells_new(ptr::null(), &mut core.cells),
                "create cell iterator",
            )?;
            check(
                ghostty_key_encoder_new(ptr::null(), &mut core.keys),
                "create key encoder",
            )?;
            check(
                ghostty_key_event_new(ptr::null(), &mut core.key_probe),
                "create key probe",
            )?;
            check(
                ghostty_mouse_encoder_new(ptr::null(), &mut core.mouse),
                "create mouse encoder",
            )?;
            check(
                ghostty_mouse_event_new(ptr::null(), &mut core.mouse_event),
                "create mouse event",
            )?;
        }
        let context = (&mut *core as *mut Core).cast::<c_void>();
        core.set_pointer(GHOSTTY_TERMINAL_OPT_USERDATA, context)?;
        callbacks::install(&mut core)?;
        core.set(
            GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES,
            &core.options.unknown_max_bytes,
        )?;
        core.set(GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT, &0_usize)?;
        core.set(
            GHOSTTY_TERMINAL_OPT_MODE_DEFAULT,
            &GhosttyTerminalModeConfig {
                mode: 2027,
                value: core.options.grapheme_width,
            },
        )?;
        if !core.options.terminfo_name.is_empty() {
            let value = native_string(core.options.terminfo_name.as_bytes());
            core.set(GHOSTTY_TERMINAL_OPT_TERMINFO_NAME, &value)?;
        }
        if let Some(value) = core.options.scrollback_max_bytes {
            core.set(GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, &value)?;
        }
        if let Some(value) = core.options.scrollback_max_lines {
            core.set(GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &value)?;
        }
        core.initialize_profile()?;
        let cols = core.options.cols;
        let rows = core.options.rows;
        let width = core.options.cell_width_px;
        let height = core.options.cell_height_px;
        let mut terminal = Self {
            core,
            _single_thread: PhantomData,
        };
        terminal.resize(cols, rows, width, height)?;
        Ok(terminal)
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        unsafe {
            ghostty_terminal_vt_write(self.core.native, bytes.as_ptr(), bytes.len());
        }
        self.core.check_callbacks()?;
        if self
            .core
            .get::<bool>(GHOSTTY_TERMINAL_DATA_VT_PROCESSING_ERROR)?
        {
            return Err(Error::Processing);
        }
        self.core.check_geometry()
    }

    pub fn resize(&mut self, cols: u16, rows: u16, width: u32, height: u32) -> Result<()> {
        valid_size(cols, rows)?;
        let previous = (
            self.core.options.cols,
            self.core.options.rows,
            self.core.options.cell_width_px,
            self.core.options.cell_height_px,
        );
        self.core.options.cols = cols;
        self.core.options.rows = rows;
        self.core.options.cell_width_px = width;
        self.core.options.cell_height_px = height;
        let result = unsafe {
            check(
                ghostty_terminal_resize(self.core.native, cols, rows, width, height),
                "resize terminal",
            )
        };
        if let Err(error) = result {
            (
                self.core.options.cols,
                self.core.options.rows,
                self.core.options.cell_width_px,
                self.core.options.cell_height_px,
            ) = previous;
            return Err(error);
        }
        self.core.dirty = vec![true; usize::from(rows)];
        self.core.visual = None;
        self.core.visual_rows.clear();
        self.core.hold_pending = false;
        self.core.check_callbacks()
    }

    pub fn release_hold(&mut self) -> Result<()> {
        self.core.set(
            GHOSTTY_TERMINAL_OPT_MODE,
            &GhosttyTerminalModeConfig {
                mode: 2026,
                value: false,
            },
        )?;
        if self.core.held {
            self.core.held = false;
            self.core.hold_pending = false;
            self.core.effects.push(Effect::RenderHold(false));
        }
        self.core.check_callbacks()
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.core.effects)
    }
    pub fn dirty_rows(&self) -> &[bool] {
        &self.core.dirty
    }
    pub fn clear_damage(&mut self) {
        self.core.dirty.fill(false);
    }
    pub fn mode(&self, mode: u16) -> Result<bool> {
        self.core.mode(mode)
    }
    pub fn snapshot(&mut self) -> Result<State> {
        self.core.snapshot()
    }
    pub fn input_state(&mut self) -> Result<InputState> {
        self.core.input_state()
    }
}

fn valid_size(cols: u16, rows: u16) -> Result<()> {
    if cols == 0 || rows == 0 || usize::from(cols) * usize::from(rows) > 1 << 20 {
        Err(Error::Dimensions { cols, rows })
    } else {
        Ok(())
    }
}

impl Core {
    fn get<T: Default>(&self, key: GhosttyTerminalData) -> Result<T> {
        let mut result = T::default();
        unsafe {
            check(
                ghostty_terminal_get(self.native, key, (&mut result as *mut T).cast()),
                "read terminal",
            )?;
        }
        Ok(result)
    }

    fn text(&self, key: GhosttyTerminalData) -> Result<Vec<u8>> {
        let value = self.get::<GhosttyString>(key)?;
        Ok(unsafe { copied(value) })
    }

    fn set<T>(&self, key: GhosttyTerminalOption, value: &T) -> Result<()> {
        self.set_pointer(key, (value as *const T).cast())
    }

    fn set_pointer(&self, key: GhosttyTerminalOption, value: *const c_void) -> Result<()> {
        unsafe {
            check(
                ghostty_terminal_set(self.native, key, value),
                "configure terminal",
            )
        }
    }

    fn mode(&self, mode: u16) -> Result<bool> {
        let mut value = GhosttyTerminalModeConfig { mode, value: false };
        unsafe {
            check(
                ghostty_terminal_get(
                    self.native,
                    GHOSTTY_TERMINAL_DATA_MODE,
                    (&mut value as *mut GhosttyTerminalModeConfig).cast(),
                ),
                "read mode",
            )?;
        }
        Ok(value.value)
    }

    fn check_geometry(&self) -> Result<()> {
        let owned = (self.options.cols, self.options.rows);
        let native = (
            self.get::<u16>(GHOSTTY_TERMINAL_DATA_COLS)?,
            self.get::<u16>(GHOSTTY_TERMINAL_DATA_ROWS)?,
        );
        if owned != native {
            Err(Error::Geometry { owned, native })
        } else {
            Ok(())
        }
    }

    fn check_callbacks(&mut self) -> Result<()> {
        self.callback_error.take().map_or(Ok(()), Err)
    }

    fn initialize_profile(&mut self) -> Result<()> {
        unsafe {
            check(
                ghostty_render_state_update(self.render, self.native),
                "initialize colors",
            )?;
        }
        let mut colors = GhosttyRenderStateColors {
            size: size_of::<GhosttyRenderStateColors>(),
            ..Default::default()
        };
        unsafe {
            check(
                ghostty_render_state_get(
                    self.render,
                    GHOSTTY_RENDER_STATE_DATA_COLORS,
                    (&mut colors as *mut GhosttyRenderStateColors).cast(),
                ),
                "read initial colors",
            )?;
        }
        let p = &self.options.profile;
        let fg = p.foreground.map(native_rgb).unwrap_or(colors.foreground);
        let bg = p.background.map(native_rgb).unwrap_or(colors.background);
        self.set(GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &fg)?;
        self.set(GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &bg)?;
        if let Some(c) = p.cursor {
            self.set(GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &native_rgb(c))?;
        }
        for (index, rgb) in &p.palette {
            colors.palette[usize::from(*index)] = native_rgb(*rgb);
        }
        self.set(GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, &colors.palette)?;
        for (mode, value) in &p.modes {
            self.set(
                GHOSTTY_TERMINAL_OPT_MODE_DEFAULT,
                &GhosttyTerminalModeConfig {
                    mode: *mode,
                    value: *value,
                },
            )?;
        }
        if let Some(style) = p.cursor_style {
            self.set(GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_STYLE, &style)?;
        }
        if let Some(blink) = p.cursor_blink {
            self.set(GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_BLINK, &blink)?;
        }
        let mut setup = b"\x1b[0 q".to_vec();
        if let Some(flags) = p.kitty_flags {
            setup.extend_from_slice(format!("\x1b[={flags};1u").as_bytes());
        }
        if let Some(mode) = p.modify_other_keys {
            setup.extend_from_slice(format!("\x1b[>4;{mode}m").as_bytes());
        }
        unsafe {
            ghostty_terminal_vt_write(self.native, setup.as_ptr(), setup.len());
        }
        self.check_callbacks()
    }
}

fn native_string(bytes: &[u8]) -> GhosttyString {
    GhosttyString {
        ptr: bytes.as_ptr(),
        len: bytes.len(),
    }
}
unsafe fn copied(value: GhosttyString) -> Vec<u8> {
    if value.len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(value.ptr, value.len) }.to_vec()
    }
}
fn native_rgb(value: Rgb) -> GhosttyColorRgb {
    GhosttyColorRgb {
        r: value.0,
        g: value.1,
        b: value.2,
    }
}
fn rgb(value: GhosttyColorRgb) -> Rgb {
    Rgb(value.r, value.g, value.b)
}

pub fn preference_modes() -> Vec<u16> {
    [
        0x8002, 0x8004, 0x800c, 0x8014, 1, 4, 5, 7, 8, 25, 45, 66, 67, 1004, 1007, 1035, 1036,
        1039, 1045, 2004, 2031, 2048,
    ]
    .to_vec()
}

pub(crate) const INPUT_MODES: &[u16] = &[
    1, 66, 67, 1035, 1036, 1039, 1004, 1007, 2004, 1005, 1006, 1015, 1016,
];
const STATE_MODES: &[u16] = &[
    0x8002, 0x8004, 0x800c, 0x8014, 3, 4, 5, 6, 7, 8, 9, 12, 25, 40, 45, 47, 69, 1000, 1002, 1003,
    1045, 1047, 1048, 1049, 2026, 2027, 2031, 2033, 2048, 5522,
];

fn read_modes(
    core: &Core,
    list: &[u16],
    modes: &mut BTreeMap<u16, bool>,
    errors: &mut BTreeMap<u16, String>,
) {
    for mode in list {
        match core.mode(*mode) {
            Ok(value) => {
                modes.insert(*mode, value);
            }
            Err(error) => {
                errors.insert(*mode, error.to_string());
            }
        }
    }
}
