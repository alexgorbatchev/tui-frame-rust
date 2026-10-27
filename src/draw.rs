//! Cached Ratatui chrome and lossless native child-cell rendering.
use crate::{
    Error, Result,
    console::{ColorProfile, Console},
    frame::{DrawContext, Frame, Geometry, Snapshot},
    terminal::{Cell, Color, Cursor, Rgb, Style},
};
use crossterm::{
    QueueableCommand,
    cursor::MoveTo,
    style::{Attribute, Print, SetAttribute},
    terminal::{Clear, ClearType},
};
use ratatui::{
    buffer::{Buffer, CellDiffOption, CellWidth},
    layout::Rect,
    style::{Color as ChromeColor, Modifier},
};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(crate) struct Damage<'a> {
    pub full: bool,
    pub regions: bool,
    pub rows: &'a [bool],
}

#[derive(Default)]
pub(crate) struct Compositor {
    area: Rect,
    cells: Vec<Cell>,
    painted: Vec<Cell>,
    cursor: Option<(Cursor, Option<Rgb>, Rect)>,
}
impl Compositor {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn paint<T: Clone + Send + 'static>(
        &mut self,
        frame: &Frame<'_, T>,
        console: &mut Console<'_>,
        snapshot: &Snapshot,
        geometry: &Geometry,
        damage: Damage<'_>,
    ) -> Result<()> {
        let full = damage.full || self.area != geometry.outer || self.cells.is_empty();
        if full {
            self.area = geometry.outer;
            self.cells =
                vec![blank(); usize::from(self.area.width) * usize::from(self.area.height)];
            border(&mut self.cells, geometry);
        }
        self.regions(frame, geometry, snapshot, full || damage.regions)?;
        self.child(snapshot, full, damage.rows);
        let cursor = (
            snapshot.terminal.cursor.clone(),
            snapshot.terminal.colors.cursor,
            snapshot.viewport,
        );
        let mut changes = Vec::new();
        for (i, cell) in self.cells.iter().enumerate() {
            if cell.width > 0
                && (full
                    || self
                        .painted
                        .get(i)
                        .is_none_or(|old| !same_visual(old, cell)))
            {
                changes.push(i);
            }
        }
        let changed = !changes.is_empty();
        if changed || self.cursor.as_ref() != Some(&cursor) {
            console.set_mode(2026, true);
            console.set_mode(25, false);
            if full {
                console
                    .buffer
                    .queue(SetAttribute(Attribute::Reset))?
                    .queue(Clear(ClearType::All))?;
            }
            for i in changes {
                let cell = &self.cells[i];
                let x = (i % usize::from(self.area.width)) as u16;
                let y = (i / usize::from(self.area.width)) as u16;
                console.buffer.queue(MoveTo(x, y))?;
                let style = output_style(
                    &cell.style,
                    console.color_profile(),
                    &console.profile.palette,
                );
                write_style(&mut console.buffer, &style)?;
                if !cell.hyperlink.is_empty() {
                    console.buffer.extend(b"\x1b]8;;");
                    console.buffer.extend(&cell.hyperlink);
                    console.buffer.extend(b"\x1b\\");
                }
                console.buffer.queue(Print(&cell.text))?;
                if !cell.hyperlink.is_empty() {
                    console.buffer.extend(b"\x1b]8;;\x1b\\");
                }
            }
            console.buffer.queue(SetAttribute(Attribute::Reset))?;
            console.sync_cursor(&snapshot.terminal);
            let native = &snapshot.terminal.cursor;
            let visible = native.visible
                && native.viewport_has_value
                && native.x < snapshot.viewport.width
                && native.y < snapshot.viewport.height;
            if visible {
                console.buffer.queue(MoveTo(
                    snapshot.viewport.x + native.x,
                    snapshot.viewport.y + native.y,
                ))?;
            }
            console.set_mode(25, visible);
            console.set_mode(2026, false);
        }
        console.flush()?;
        if changed {
            self.painted.clone_from(&self.cells);
        }
        self.cursor = Some(cursor);
        Ok(())
    }
    fn regions<T: Clone + Send + 'static>(
        &mut self,
        frame: &Frame<'_, T>,
        geometry: &Geometry,
        snapshot: &Snapshot,
        force: bool,
    ) -> Result<()> {
        for (index, area) in geometry.regions.iter().copied().enumerate() {
            let work = {
                let mut config = frame
                    .config
                    .lock()
                    .map_err(|_| Error::Session("frame configuration lock poisoned".into()))?;
                let region = &mut config.regions[index];
                let paint = region.draw.is_some()
                    && (force
                        || region.dirty
                        || region.canvas.as_ref().is_none_or(|b| {
                            b.area.width != area.width || b.area.height != area.height
                        }));
                if paint {
                    region.dirty = false;
                    Some((
                        region.draw.clone(),
                        region.data.clone(),
                        region.canvas.take(),
                    ))
                } else {
                    None
                }
            };
            let Some((draw, data, canvas)) = work else {
                continue;
            };
            let Some(draw) = draw else {
                continue;
            };
            if area.is_empty() {
                continue;
            }
            let local = Rect::new(0, 0, area.width, area.height);
            let mut canvas = canvas
                .filter(|b| b.area == local)
                .unwrap_or_else(|| Buffer::empty(local));
            canvas.reset();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let mut callback = draw
                    .lock()
                    .map_err(|_| Error::Session("frame drawing callback lock poisoned".into()))?;
                callback(DrawContext {
                    term: snapshot.clone(),
                    view: &mut canvas,
                    data,
                });
                Ok::<(), Error>(())
            }));
            match outcome {
                Ok(result) => result?,
                Err(panic) => {
                    return Err(Error::Session(format!(
                        "frame drawing callback panicked: {}",
                        crate::observation::panic_message(&panic)
                    )));
                }
            }
            self.chrome(&canvas, area);
            frame
                .config
                .lock()
                .map_err(|_| Error::Session("frame configuration lock poisoned".into()))?
                .regions[index]
                .canvas = Some(canvas);
        }
        Ok(())
    }
    fn chrome(&mut self, canvas: &Buffer, area: Rect) {
        for y in 0..area.height {
            let mut x = 0;
            while x < area.width {
                let cell = &canvas[(x, y)];
                let safe = !cell.symbol().chars().any(char::is_control);
                let width = if safe { cell.cell_width() } else { 0 };
                if width == 0
                    || width > u16::from(u8::MAX)
                    || width > area.width - x
                    || cell.diff_option == CellDiffOption::Skip
                {
                    self.put(area.x + x, area.y + y, blank());
                    x += 1;
                    continue;
                }
                let style = chrome_style(cell);
                self.put(
                    area.x + x,
                    area.y + y,
                    Cell {
                        text: cell.symbol().into(),
                        width: u8::try_from(width).unwrap_or(u8::MAX),
                        style: style.clone(),
                        ..Default::default()
                    },
                );
                for dx in 1..width {
                    self.put(
                        area.x + x + dx,
                        area.y + y,
                        Cell {
                            style: style.clone(),
                            ..Default::default()
                        },
                    );
                }
                x += width;
            }
        }
    }
    fn child(&mut self, snapshot: &Snapshot, full: bool, dirty: &[bool]) {
        let area = snapshot.viewport;
        let cols = usize::from(snapshot.terminal.input.cols);
        let rows = snapshot.terminal.input.rows;
        for y in 0..area.height {
            if !full && !dirty.get(usize::from(y)).copied().unwrap_or(false) {
                continue;
            }
            let mut x = 0;
            while x < area.width {
                let index = usize::from(y) * cols + usize::from(x);
                let cell = if y < rows && usize::from(x) < cols {
                    snapshot.terminal.cells.get(index)
                } else {
                    None
                };
                if let Some(cell) =
                    cell.filter(|c| c.width > 0 && u16::from(c.width) <= area.width - x)
                {
                    self.put(area.x + x, area.y + y, cell.clone());
                    for dx in 1..u16::from(cell.width) {
                        self.put(
                            area.x + x + dx,
                            area.y + y,
                            Cell {
                                style: cell.style.clone(),
                                ..Default::default()
                            },
                        );
                    }
                    x += u16::from(cell.width);
                } else {
                    self.put(area.x + x, area.y + y, blank());
                    x += 1;
                }
            }
        }
    }
    fn put(&mut self, x: u16, y: u16, cell: Cell) {
        if self.area.contains((x, y).into()) {
            self.cells[usize::from(y) * usize::from(self.area.width) + usize::from(x)] = cell;
        }
    }
}
fn blank() -> Cell {
    Cell {
        text: " ".into(),
        width: 1,
        ..Default::default()
    }
}
fn same_visual(a: &Cell, b: &Cell) -> bool {
    a.text == b.text && a.width == b.width && a.style == b.style && a.hyperlink == b.hyperlink
}
fn border(cells: &mut [Cell], g: &Geometry) {
    if g.border == g.child {
        return;
    }
    let mut put = |x: u16, y: u16, text: &str| {
        cells[usize::from(y) * usize::from(g.outer.width) + usize::from(x)] = Cell {
            text: text.into(),
            width: 1,
            ..Default::default()
        };
    };
    let r = g.border;
    let right = r.right() - 1;
    let bottom = r.bottom() - 1;
    for x in r.x + 1..right {
        put(x, r.y, "─");
        put(x, bottom, "─");
    }
    for y in r.y + 1..bottom {
        put(r.x, y, "│");
        put(right, y, "│");
    }
    put(r.x, r.y, "╭");
    put(right, r.y, "╮");
    put(r.x, bottom, "╰");
    put(right, bottom, "╯");
}
fn output_color(color: Color) -> Option<anstyle::Color> {
    match color {
        Color::Default => None,
        Color::Palette(index) => {
            let color = anstyle::Ansi256Color(index);
            Some(color.into_ansi().map_or_else(|| color.into(), Into::into))
        }
        Color::Rgb(Rgb(r, g, b)) => Some(anstyle::RgbColor(r, g, b).into()),
    }
}

fn output_style(
    style: &Style,
    profile: ColorProfile,
    host: &std::collections::BTreeMap<u8, Rgb>,
) -> Style {
    let mut result = style.clone();
    let mut palette = anstyle_lossy::palette::DEFAULT;
    for (&index, &Rgb(r, g, b)) in host.range(..16) {
        palette.0[usize::from(index)] = anstyle::RgbColor(r, g, b);
    }
    let convert = |color| match (profile, color) {
        (ColorProfile::None, _) => Color::Default,
        (_, Color::Default) => Color::Default,
        (ColorProfile::TrueColor, color) => color,
        (ColorProfile::Ansi256, Color::Palette(index)) => Color::Palette(index),
        (ColorProfile::Ansi, Color::Palette(index @ 0..16)) => Color::Palette(index),
        (ColorProfile::Ansi, Color::Palette(index)) => Color::Palette(
            anstyle_lossy::xterm_to_ansi(anstyle::Ansi256Color(index), palette) as u8,
        ),
        (ColorProfile::Ansi256, Color::Rgb(Rgb(r, g, b))) => {
            Color::Palette(anstyle_lossy::rgb_to_xterm(anstyle::RgbColor(r, g, b)).index())
        }
        (ColorProfile::Ansi, Color::Rgb(Rgb(r, g, b))) => {
            Color::Palette(anstyle_lossy::rgb_to_ansi(anstyle::RgbColor(r, g, b), palette) as u8)
        }
    };
    result.foreground = convert(result.foreground);
    result.background = convert(result.background);
    result.underline_color = convert(result.underline_color);
    result
}
fn write_style(output: &mut Vec<u8>, style: &Style) -> Result<()> {
    output.queue(SetAttribute(Attribute::Reset))?;
    anstyle::Style::new()
        .fg_color(output_color(style.foreground))
        .bg_color(output_color(style.background))
        .underline_color(output_color(style.underline_color))
        .write_to(output)?;
    for (enabled, attribute) in [
        (style.bold, Attribute::Bold),
        (style.faint, Attribute::Dim),
        (style.italic, Attribute::Italic),
        (style.blink, Attribute::SlowBlink),
        (style.inverse, Attribute::Reverse),
        (style.invisible, Attribute::Hidden),
        (style.strikethrough, Attribute::CrossedOut),
        (style.overline, Attribute::OverLined),
    ] {
        if enabled {
            output.queue(SetAttribute(attribute))?;
        }
    }
    let underline = match style.underline {
        1 => Some(Attribute::Underlined),
        2 => Some(Attribute::DoubleUnderlined),
        3 => Some(Attribute::Undercurled),
        4 => Some(Attribute::Underdotted),
        5 => Some(Attribute::Underdashed),
        _ => None,
    };
    if let Some(attribute) = underline {
        output.queue(SetAttribute(attribute))?;
    }
    Ok(())
}
fn chrome_color(color: ChromeColor) -> Color {
    match color {
        ChromeColor::Reset => Color::Default,
        ChromeColor::Rgb(r, g, b) => Color::Rgb(Rgb(r, g, b)),
        ChromeColor::Indexed(index) => Color::Palette(index),
        ChromeColor::Black => Color::Palette(0),
        ChromeColor::Red => Color::Palette(1),
        ChromeColor::Green => Color::Palette(2),
        ChromeColor::Yellow => Color::Palette(3),
        ChromeColor::Blue => Color::Palette(4),
        ChromeColor::Magenta => Color::Palette(5),
        ChromeColor::Cyan => Color::Palette(6),
        ChromeColor::Gray => Color::Palette(7),
        ChromeColor::DarkGray => Color::Palette(8),
        ChromeColor::LightRed => Color::Palette(9),
        ChromeColor::LightGreen => Color::Palette(10),
        ChromeColor::LightYellow => Color::Palette(11),
        ChromeColor::LightBlue => Color::Palette(12),
        ChromeColor::LightMagenta => Color::Palette(13),
        ChromeColor::LightCyan => Color::Palette(14),
        ChromeColor::White => Color::Palette(15),
    }
}
fn chrome_style(cell: &ratatui::buffer::Cell) -> Style {
    let m = cell.modifier;
    Style {
        foreground: chrome_color(cell.fg),
        background: chrome_color(cell.bg),
        bold: m.contains(Modifier::BOLD),
        italic: m.contains(Modifier::ITALIC),
        faint: m.contains(Modifier::DIM),
        blink: m.intersects(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK),
        inverse: m.contains(Modifier::REVERSED),
        invisible: m.contains(Modifier::HIDDEN),
        strikethrough: m.contains(Modifier::CROSSED_OUT),
        underline: i32::from(m.contains(Modifier::UNDERLINED)),
        ..Default::default()
    }
}

#[cfg(test)]
#[path = "../tests/unit/drawing.rs"]
mod tests;
