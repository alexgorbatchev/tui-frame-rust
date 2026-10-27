use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    #[default]
    Default,
    Palette(u8),
    Rgb(Rgb),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub foreground: Color,
    pub background: Color,
    pub underline_color: Color,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    /// Native SGR underline style: none, single, double, curly, dotted, dashed.
    pub underline: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub width: u8,
    pub style: Style,
    pub hyperlink: Vec<u8>,
    pub raw: u64,
    pub wide: u32,
    pub style_index: usize,
    pub selected: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub viewport_has_value: bool,
    pub wide_tail: bool,
    pub visible: bool,
    pub blinking: bool,
    pub password_input: bool,
    pub style: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Colors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<Rgb>,
    pub palette: [Rgb; 256],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Visual {
    pub cells: Vec<Cell>,
    pub styles: Vec<Style>,
    pub cursor: Cursor,
    pub colors: Colors,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputState {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub held: bool,
    pub alternate: bool,
    pub modes: BTreeMap<u16, bool>,
    pub mode_errors: BTreeMap<u16, String>,
    pub kitty_flags: u8,
    pub modify_other_keys_2: bool,
    pub mouse_tracking: bool,
    /// Ghostty's active tracking mode, independently of the retained mode bits.
    pub mouse_tracking_mode: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    pub compression_supported: bool,
    pub primary_pages: u64,
    pub primary_virtual_bytes: u64,
    pub primary_resident_bytes: u64,
    pub primary_compressed_pages: u64,
    pub primary_compressed_bytes: u64,
    pub primary_image_bytes: u64,
    pub alternate_pages: u64,
    pub alternate_virtual_bytes: u64,
    pub alternate_resident_bytes: u64,
    pub alternate_compressed_pages: u64,
    pub alternate_compressed_bytes: u64,
    pub alternate_image_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub input: InputState,
    pub cells: Vec<Cell>,
    /// Index zero is the default native style. Indices belong to this state.
    pub native_styles: Vec<Style>,
    pub cursor: Cursor,
    pub colors: Colors,
    pub title: Vec<u8>,
    pub directory: Vec<u8>,
    pub mouse_shape: u32,
    pub scrollbar: (u64, u64, u64),
    pub scrollback_rows: usize,
    pub total_rows: usize,
    pub scrollback_max_bytes: Option<usize>,
    pub scrollback_max_lines: Option<usize>,
    pub memory: MemoryUsage,
    pub vt_ground: bool,
    pub vt_processing_error: bool,
    pub viewport_active: bool,
    pub cursor_pending_wrap: bool,
    pub cursor_at_prompt: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub palette: BTreeMap<u8, Rgb>,
    pub modes: BTreeMap<u16, bool>,
    pub cursor_style: Option<u32>,
    pub cursor_blink: Option<bool>,
    pub scheme: Option<u32>,
    pub kitty_flags: Option<u8>,
    pub modify_other_keys: Option<u8>,
}

pub struct Options {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub profile: Profile,
    pub terminfo_name: String,
    pub version: String,
    pub grapheme_width: bool,
    pub unknown_max_bytes: usize,
    pub scrollback_max_bytes: Option<usize>,
    pub scrollback_max_lines: Option<usize>,
    /// Preserve inherited palette references for ANSI/ANSI256 outer renderers.
    pub indexed_entries: usize,
    pub clipboard_read: Option<Box<dyn FnMut(ClipboardRead) -> ClipboardReadReply>>,
    pub clipboard_write: Option<Box<dyn FnMut(ClipboardWrite) -> ClipboardWriteReply>>,
}

impl Options {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cell_width_px: 0,
            cell_height_px: 0,
            profile: Profile::default(),
            terminfo_name: String::new(),
            version: "tui-frame".into(),
            grapheme_width: false,
            unknown_max_bytes: 64 << 10,
            scrollback_max_bytes: None,
            scrollback_max_lines: None,
            indexed_entries: 0,
            clipboard_read: None,
            clipboard_write: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardContent {
    pub mime: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardRead {
    pub location: u32,
    pub mimes: Vec<Vec<u8>>,
    pub list: bool,
    pub name: Vec<u8>,
    pub granted: bool,
    pub can_remember: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardWrite {
    pub location: u32,
    pub contents: Vec<ClipboardContent>,
    pub name: Vec<u8>,
    pub granted: bool,
    pub can_remember: bool,
}

#[derive(Clone, Debug)]
pub struct ClipboardReadReply {
    /// Native result: success=0, denied=1, unsupported=2, busy=3, IO error=4.
    pub result: u32,
    pub contents: Vec<ClipboardContent>,
    pub available: Vec<Vec<u8>>,
    pub remember: bool,
}

#[derive(Clone, Debug)]
pub struct ClipboardWriteReply {
    /// Native result: success=0, denied=1, unsupported=2, busy=3,
    /// invalid data=4, IO error=5.
    pub result: u32,
    pub remember: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Reply(Vec<u8>),
    Bell,
    Title(Vec<u8>),
    Directory(Vec<u8>),
    Progress {
        state: u32,
        progress: i8,
    },
    Notification {
        title: Vec<u8>,
        body: Vec<u8>,
    },
    Unknown {
        tag: u32,
        content: Vec<u8>,
        truncated: bool,
        terminator: Option<u32>,
    },
    RenderHold(bool),
    SemanticPrompt {
        kind: u32,
        prompt_kind: u32,
        exit_code: Option<i32>,
        command: Vec<u8>,
        error: Vec<u8>,
    },
    Reset,
    ClipboardRead(ClipboardRead),
    ClipboardWrite(ClipboardWrite),
}

#[derive(Clone, Copy, Debug)]
pub struct Mouse {
    /// Native action: press=0, release=1, motion=2.
    pub action: u32,
    /// Native button identity (left=1, right=2, middle=3, wheel=4..7).
    pub button: Option<u32>,
    pub modifiers: u16,
    pub x: f32,
    pub y: f32,
}
