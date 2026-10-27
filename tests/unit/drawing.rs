use crate::{
    console::Console,
    draw::{Compositor, Damage},
    frame::{ChildSnapshot, Edge, Frame, PtySnapshot, Snapshot},
    process,
    terminal::{Options, Terminal},
};
use portable_pty::CommandBuilder;
use ratatui::{layout::Rect, style::Color};
use std::{
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    sync::{Arc, Mutex},
    time::SystemTime,
};

fn pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}
fn capture(master: &OwnedFd) -> Vec<u8> {
    let mut output = Vec::new();
    let mut bytes = [0; 65536];
    loop {
        let mut fd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut fd, 1, 0) } <= 0 {
            break;
        }
        let n = unsafe { libc::read(master.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        assert!(n > 0);
        output.extend(&bytes[..n as usize]);
    }
    output
}
fn snapshot(term: &mut Terminal, viewport: Rect, outer: Rect) -> Snapshot {
    let pid = std::process::id() as i32;
    let sample = process::read(pid);
    let now = SystemTime::now();
    Snapshot {
        terminal: term.snapshot().unwrap(),
        viewport,
        outer,
        observed_at: now,
        child: ChildSnapshot {
            pid,
            executable: "sh".into(),
            args: Vec::new(),
            environment: Vec::new(),
            directory: ".".into(),
            started_at: now,
            process_state: None,
            operating_system: sample.clone(),
            session_processes: vec![sample],
            session_error: None,
            pty: PtySnapshot {
                window: None,
                settings: None,
                foreground_group: process::Field::available(pid, "test PTY"),
                window_error: None,
                settings_error: None,
                observed_at: now,
            },
        },
    }
}

#[test]
fn native_composition_preserves_regions_styles_links_damage_and_cursor() {
    let (master, slave) = pty();
    let mut console = Console::new(slave.as_fd(), slave.as_fd(), false).unwrap();
    console.set_color_profile(crate::console::ColorProfile::TrueColor);
    let draws = Arc::new(Mutex::new(0));
    let seen = draws.clone();
    let frame = Frame::new(CommandBuilder::new("sh"), "header".to_owned());
    frame
        .header(1, move |ctx| {
            *seen.lock().unwrap() += 1;
            ctx.view.set_string(
                0,
                0,
                &ctx.data,
                ratatui::style::Style::default().fg(Color::Red),
            );
        })
        .unwrap();
    frame
        .footer(1, |ctx| {
            ctx.view
                .set_string(0, 0, "footer", ratatui::style::Style::default());
        })
        .unwrap();
    frame.set_border(true).unwrap();
    let geometry = frame.layout(12, 8).unwrap();
    let mut child =
        Terminal::new(Options::new(geometry.child.width, geometry.child.height)).unwrap();
    child.write(b"\x1b[1;2;3;4:3;5;7;8;9;53;38;2;10;20;30;48;2;40;50;60;58;2;70;80;90m\x1b]8;;https://example.test\x1b\\C\x1b]8;;\x1b\\\x1b[0m\x1b[2;2H").unwrap();
    let mut compositor = Compositor::new();
    let snap = snapshot(&mut child, geometry.child, geometry.outer);
    compositor
        .paint(
            &frame,
            &mut console,
            &snap,
            &geometry,
            Damage {
                full: true,
                regions: true,
                rows: &[],
            },
        )
        .unwrap();
    let raw = capture(&master);
    assert!(!raw.is_empty());
    let mut outer = Terminal::new(Options::new(12, 8)).unwrap();
    outer.write(&raw).unwrap();
    let state = outer.snapshot().unwrap();
    assert_eq!(state.cells[0].text, "h");
    assert_eq!(state.cells[12].text, "╭");
    assert_eq!(state.cells[7 * 12].text, "f");
    let cell = &state.cells[2 * 12 + 1];
    assert_eq!(cell.text, "C");
    assert_eq!(cell.style, snap.terminal.cells[0].style);
    assert_eq!(cell.hyperlink, b"https://example.test");
    assert_eq!(
        (state.cursor.x, state.cursor.y),
        (geometry.child.x + 1, geometry.child.y + 1)
    );
    compositor
        .paint(
            &frame,
            &mut console,
            &snap,
            &geometry,
            Damage {
                full: false,
                regions: false,
                rows: &[],
            },
        )
        .unwrap();
    assert!(capture(&master).is_empty());
    assert_eq!(*draws.lock().unwrap(), 1);
    child.write(b"\x1b[H\x1b[2K").unwrap();
    let updated = snapshot(&mut child, geometry.child, geometry.outer);
    compositor
        .paint(
            &frame,
            &mut console,
            &updated,
            &geometry,
            Damage {
                full: false,
                regions: false,
                rows: &[true],
            },
        )
        .unwrap();
    let raw = capture(&master);
    outer.write(&raw).unwrap();
    let state = outer.snapshot().unwrap();
    assert_eq!(state.cells[2 * 12 + 1].text, " ");
    assert_eq!(state.cells[0].text, "h");
    assert_eq!(*draws.lock().unwrap(), 1);
    frame.invalidate(Edge::Header, "ready".into()).unwrap();
    compositor
        .paint(
            &frame,
            &mut console,
            &updated,
            &geometry,
            Damage {
                full: false,
                regions: false,
                rows: &[],
            },
        )
        .unwrap();
    outer.write(&capture(&master)).unwrap();
    let state = outer.snapshot().unwrap();
    assert_eq!(state.cells[0].text, "r");
    assert_eq!(state.cells[5].text, " ");
    assert_eq!(*draws.lock().unwrap(), 2);
}

#[test]
fn region_callbacks_keep_owned_snapshots_reuse_storage_and_retain_invalidation() {
    let (master, slave) = pty();
    let mut console = Console::new(slave.as_fd(), slave.as_fd(), false).unwrap();
    let frame = Frame::new(CommandBuilder::new("sh"), "initial".to_owned());
    let control = frame.controller();
    let saved = Arc::new(Mutex::new(Vec::new()));
    let output = saved.clone();
    frame
        .header(1, move |ctx| {
            let mut values = output.lock().unwrap();
            values.push((
                ctx.view.content.as_ptr() as usize,
                ctx.term.clone(),
                ctx.data.clone(),
            ));
            if values.len() == 1 {
                ctx.view
                    .set_string(0, 0, "long header", ratatui::style::Style::default());
                control.invalidate(Edge::Header, "during".into()).unwrap();
            } else {
                ctx.view
                    .set_string(0, 0, &ctx.data, ratatui::style::Style::default());
            }
        })
        .unwrap();
    let geometry = frame.layout(14, 4).unwrap();
    let mut child = Terminal::new(Options::new(14, 3)).unwrap();
    child.write(b"A").unwrap();
    let first = snapshot(&mut child, geometry.child, geometry.outer);
    let mut compositor = Compositor::new();
    compositor
        .paint(
            &frame,
            &mut console,
            &first,
            &geometry,
            Damage {
                full: true,
                regions: false,
                rows: &[],
            },
        )
        .unwrap();
    assert!(frame.region_demand().1, "callback invalidation was erased");
    child.write(b"\x1b[HB").unwrap();
    let second = snapshot(&mut child, geometry.child, geometry.outer);
    compositor
        .paint(
            &frame,
            &mut console,
            &second,
            &geometry,
            Damage {
                full: false,
                regions: false,
                rows: &[true],
            },
        )
        .unwrap();
    let values = saved.lock().unwrap();
    assert_eq!(values.len(), 2);
    assert_eq!(values[0].0, values[1].0, "canvas storage reallocated");
    assert_eq!(values[0].1.terminal.cells[0].text, "A");
    assert_eq!(values[1].1.terminal.cells[0].text, "B");
    assert_eq!(values[1].2, "during");
    assert_eq!(
        compositor.cells[6].text, " ",
        "reused canvas retained old header"
    );
    assert!(!frame.region_demand().1);
    let mut outer = Terminal::new(Options::new(14, 4)).unwrap();
    outer.write(&capture(&master)).unwrap();
    assert_eq!(outer.snapshot().unwrap().cells[0].text, "d");
}

#[test]
fn draw_panics_are_reported_before_any_partial_frame_is_written() {
    let (master, slave) = pty();
    let mut console = Console::new(slave.as_fd(), slave.as_fd(), false).unwrap();
    let frame = Frame::new(CommandBuilder::new("sh"), ());
    frame.header(1, |_| panic!("draw failed")).unwrap();
    let geometry = frame.layout(8, 4).unwrap();
    let mut child = Terminal::new(Options::new(8, 3)).unwrap();
    let snap = snapshot(&mut child, geometry.child, geometry.outer);
    let error = Compositor::new()
        .paint(
            &frame,
            &mut console,
            &snap,
            &geometry,
            Damage {
                full: true,
                regions: true,
                rows: &[],
            },
        )
        .unwrap_err();
    assert!(matches!(error, crate::Error::Session(ref message) if message.contains("draw failed")));
    assert!(capture(&master).is_empty());
}

#[test]
fn chrome_wide_graphemes_and_clipped_or_invalid_cells_cannot_escape_the_region() {
    use ratatui::buffer::{Buffer, CellDiffOption};
    use std::num::NonZeroU16;
    let mut compositor = Compositor::new();
    compositor.area = Rect::new(0, 0, 300, 2);
    compositor.cells = vec![super::blank(); 600];
    let mut canvas = Buffer::empty(Rect::new(0, 0, 300, 1));
    canvas[(0, 0)].set_symbol("👩‍💻");
    canvas[(2, 0)].set_symbol("\x1b[31m");
    canvas[(3, 0)]
        .set_symbol("skip")
        .set_diff_option(CellDiffOption::Skip);
    canvas[(4, 0)]
        .set_symbol("huge")
        .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::new(256).unwrap()));
    canvas[(299, 0)].set_symbol("界");
    compositor.chrome(&canvas, Rect::new(0, 0, 300, 1));
    assert_eq!(compositor.cells[0].text, "👩‍💻");
    assert_eq!(compositor.cells[0].width, 2);
    assert_eq!(compositor.cells[1].width, 0);
    for x in [2, 3, 4, 299] {
        assert_eq!(compositor.cells[x].text, " ", "invalid cell at {x} leaked");
    }
    assert_eq!(compositor.cells[300].text, " ");
}

#[test]
fn native_style_encoding_replays_every_underline_and_color_reduction_tier() {
    use crate::console::ColorProfile;
    use crate::terminal::{Color as NativeColor, Rgb, Style};
    let host = [(1, Rgb(1, 2, 3))].into_iter().collect();
    let mut term = Terminal::new(Options::new(12, 2)).unwrap();
    for underline in 0..=5 {
        let style = Style {
            foreground: NativeColor::Rgb(Rgb(255, 0, 0)),
            background: NativeColor::Palette(200),
            underline_color: NativeColor::Palette(1),
            underline,
            bold: true,
            ..Default::default()
        };
        for profile in [
            ColorProfile::None,
            ColorProfile::Ansi,
            ColorProfile::Ansi256,
            ColorProfile::TrueColor,
        ] {
            let converted = super::output_style(&style, profile, &host);
            assert!(converted.bold);
            assert_eq!(converted.underline, underline);
            match profile {
                ColorProfile::None => assert_eq!(
                    (
                        converted.foreground,
                        converted.background,
                        converted.underline_color
                    ),
                    (
                        NativeColor::Default,
                        NativeColor::Default,
                        NativeColor::Default
                    )
                ),
                ColorProfile::Ansi => {
                    assert!(matches!(converted.foreground, NativeColor::Palette(0..=15)));
                    assert!(matches!(converted.background, NativeColor::Palette(0..=15)));
                    assert_eq!(converted.underline_color, NativeColor::Palette(1));
                }
                ColorProfile::Ansi256 => {
                    assert_eq!(converted.foreground, NativeColor::Palette(196));
                    assert_eq!(converted.background, NativeColor::Palette(200));
                }
                ColorProfile::TrueColor => assert_eq!(converted, style),
            }
            let mut bytes = b"\x1b[H".to_vec();
            super::write_style(&mut bytes, &converted).unwrap();
            bytes.push(b'X');
            term.write(&bytes).unwrap();
            let state = term.snapshot().unwrap();
            assert_eq!(
                state.native_styles[state.cells[0].style_index],
                converted,
                "bytes {:?}; cell {:?}",
                String::from_utf8_lossy(&bytes),
                state.cells[0]
            );
        }
    }
    let style = Style {
        foreground: NativeColor::Rgb(Rgb(1, 2, 3)),
        ..Default::default()
    };
    assert_eq!(
        super::output_style(&style, ColorProfile::Ansi, &host).foreground,
        NativeColor::Palette(1)
    );
}

#[test]
fn ratatui_chrome_colors_and_modifiers_survive_native_replay_and_resize() {
    use ratatui::style::Modifier;
    let colors = [
        Color::Reset,
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
        Color::Rgb(1, 2, 3),
        Color::Indexed(200),
    ];
    let (master, slave) = pty();
    let mut console = Console::new(slave.as_fd(), slave.as_fd(), false).unwrap();
    console.set_color_profile(crate::console::ColorProfile::TrueColor);
    let frame = Frame::new(CommandBuilder::new("sh"), ());
    let dimensions = Arc::new(Mutex::new(Vec::new()));
    let observed = dimensions.clone();
    frame
        .header(1, move |ctx| {
            observed.lock().unwrap().push(ctx.view.area.width);
            for (x, color) in colors.into_iter().enumerate() {
                ctx.view[(x as u16, 0)]
                    .set_symbol("X")
                    .set_fg(color)
                    .set_bg(color)
                    .set_style(ratatui::style::Style::default().add_modifier(Modifier::all()));
            }
        })
        .unwrap();
    let geometry = frame.layout(24, 4).unwrap();
    let mut child = Terminal::new(Options::new(24, 3)).unwrap();
    let mut compositor = Compositor::new();
    let snap = snapshot(&mut child, geometry.child, geometry.outer);
    compositor
        .paint(
            &frame,
            &mut console,
            &snap,
            &geometry,
            Damage {
                full: true,
                regions: true,
                rows: &[],
            },
        )
        .unwrap();
    let mut outer = Terminal::new(Options::new(24, 4)).unwrap();
    outer.write(&capture(&master)).unwrap();
    let state = outer.snapshot().unwrap();
    for (x, color) in colors.into_iter().enumerate() {
        let style = &state.cells[x].style;
        let expected = match super::chrome_color(color) {
            crate::terminal::Color::Default => state.colors.foreground,
            crate::terminal::Color::Palette(i) => state.colors.palette[usize::from(i)],
            crate::terminal::Color::Rgb(rgb) => rgb,
        };
        assert_eq!(style.foreground, crate::terminal::Color::Rgb(expected));
        assert!(
            style.bold
                && style.faint
                && style.italic
                && style.blink
                && style.inverse
                && style.invisible
                && style.strikethrough
        );
        assert_eq!(style.underline, 1);
    }
    let wider = frame.layout(26, 4).unwrap();
    let snap = snapshot(&mut child, wider.child, wider.outer);
    compositor
        .paint(
            &frame,
            &mut console,
            &snap,
            &wider,
            Damage {
                full: false,
                regions: false,
                rows: &[],
            },
        )
        .unwrap();
    assert_eq!(*dimensions.lock().unwrap(), [24, 26]);
    assert_eq!(compositor.cells[25].text, " ");
    assert!(!capture(&master).is_empty());
}

#[test]
fn child_wide_cells_clip_clear_missing_rows_and_respect_row_damage() {
    let mut child = Terminal::new(Options::new(5, 2)).unwrap();
    child.write("A界B".as_bytes()).unwrap();
    let mut snap = snapshot(&mut child, Rect::new(1, 1, 4, 3), Rect::new(0, 0, 7, 5));
    let mut compositor = Compositor::new();
    compositor.area = snap.outer;
    compositor.cells = vec![super::blank(); 35];
    compositor.child(&snap, true, &[]);
    assert_eq!(compositor.cells[9].text, "界");
    assert_eq!(compositor.cells[9].width, 2);
    assert_eq!(compositor.cells[10].width, 0);
    assert_eq!(compositor.cells[11].text, "B");
    snap.viewport.width = 2;
    compositor.child(&snap, true, &[]);
    assert_eq!(
        compositor.cells[9].text, " ",
        "partial wide glyph escaped viewport"
    );
    snap.terminal.cells[0].text = "changed".into();
    compositor.child(&snap, false, &[false]);
    assert_eq!(compositor.cells[8].text, "A");
    snap.terminal.cells.clear();
    compositor.child(&snap, false, &[true, true, true]);
    assert_eq!(compositor.cells[8].text, " ");
    assert_eq!(
        compositor.cells[22].text, " ",
        "row beyond source was retained"
    );
}
