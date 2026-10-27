use tui_frame::{
    Error,
    terminal::{Color, Effect, Mouse, Options, Rgb, Terminal},
};

#[test]
fn native_screen_queries_and_owned_snapshots() {
    let mut term = Terminal::new(Options::new(8, 3)).unwrap();
    term.write(b"hi\x1b[6n\x1b]2;title\x07\x1b]9999;unknown\x07")
        .unwrap();
    let first = term.snapshot().unwrap();
    assert_eq!(first.cells[0].text, "h");
    assert_eq!(first.title, b"title");
    let effects = term.take_effects();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Reply(v) if v == b"\x1b[1;3R"))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Unknown { content, .. } if content == b"9999;unknown"))
    );
    term.write(b"\rnew").unwrap();
    assert_eq!(first.cells[0].text, "h");
    assert_eq!(term.snapshot().unwrap().cells[0].text, "n");
}

#[test]
fn synchronized_output_preserves_frame_with_live_metadata() {
    let mut term = Terminal::new(Options::new(8, 3)).unwrap();
    term.write(b"old\x1b[?2026h\rnew\x1b]2;live\x07").unwrap();
    let held = term.snapshot().unwrap();
    assert!(held.input.held);
    assert_eq!(held.cells[0].text, "o");
    assert_eq!(held.title, b"live");
    term.release_hold().unwrap();
    assert_eq!(term.snapshot().unwrap().cells[0].text, "n");
}

#[test]
fn fragmented_unicode_retains_native_styles_and_owned_links() {
    let mut term = Terminal::new(Options::new(12, 3)).unwrap();
    for fragment in [
        b"\x1b[2;".as_slice(),
        b"3H",
        b"\x1b[1;3;4:3;5;8;9;53;38;2;10;20;30;58;2;40;50;60m",
        b"e",
        b"\xcc",
        b"\x81",
        b"\xe7",
        b"\x95\x8c",
    ] {
        term.write(fragment).unwrap();
    }
    let first = term.snapshot().unwrap();
    let cell = &first.cells[14];
    assert_eq!((&cell.text, cell.width), (&"e\u{301}".to_owned(), 1));
    assert_eq!(
        (
            &first.cells[15].text,
            first.cells[15].width,
            first.cells[16].width
        ),
        (&"界".to_owned(), 2, 0)
    );
    assert!(
        cell.style.bold
            && cell.style.italic
            && cell.style.blink
            && cell.style.invisible
            && cell.style.strikethrough
            && cell.style.overline
    );
    assert_eq!(cell.style.foreground, Color::Rgb(Rgb(10, 20, 30)));
    assert_eq!(cell.style.underline_color, Color::Rgb(Rgb(40, 50, 60)));
    assert_eq!(cell.style.underline, 3);
    assert!(first.native_styles[cell.style_index].overline);
    term.write(b"\x1b[H\x1b]8;;https://example.test/path\x1b\\link\x1b]8;;\x1b\\")
        .unwrap();
    let linked = term.snapshot().unwrap();
    assert_eq!(linked.cells[0].hyperlink, b"https://example.test/path");
    term.write(b"\x1b[?2026h\x1b[Hnext").unwrap();
    assert_eq!(
        term.snapshot().unwrap().cells[0].hyperlink,
        linked.cells[0].hyperlink
    );
    term.release_hold().unwrap();
    assert_eq!(first.cells[14].text, "e\u{301}");
}

#[test]
fn query_callbacks_preserve_order_and_resize_uses_new_geometry() {
    let mut term = Terminal::new(Options::new(12, 3)).unwrap();
    term.write(b"\x1b[2;4H\x1b[6n\x07\x1b[?2004h\x1b[?2004$p\x1b[>1u\x1b[?u\x1b[18t")
        .unwrap();
    assert_eq!(
        term.take_effects(),
        vec![
            Effect::Reply(b"\x1b[2;4R".to_vec()),
            Effect::Bell,
            Effect::Reply(b"\x1b[?2004;1$y".to_vec()),
            Effect::Reply(b"\x1b[?1u".to_vec()),
            Effect::Reply(b"\x1b[8;3;12t".to_vec())
        ]
    );
    assert!(term.take_effects().is_empty());
    term.write(b"1\r\n2\r\n3\r\n4\r\n5").unwrap();
    let scroll = term.snapshot().unwrap();
    assert!(scroll.scrollback_rows >= 2);
    assert_eq!(scroll.total_rows, scroll.scrollback_rows + 3);
    assert_eq!(scroll.scrollbar.2, 3);
    term.resize(10, 4, 9, 18).unwrap();
    term.write(b"\x1b[14t\x1b[16t\x1b[18t").unwrap();
    assert_eq!(
        term.take_effects(),
        vec![
            Effect::Reply(b"\x1b[4;72;90t".to_vec()),
            Effect::Reply(b"\x1b[6;18;9t".to_vec()),
            Effect::Reply(b"\x1b[8;4;10t".to_vec())
        ]
    );
}

#[test]
fn mode_transitions_reset_and_geometry_error_are_explicit() {
    let mut term = Terminal::new(Options::new(12, 3)).unwrap();
    term.write(
        b"normal\x1b]2;child\x07\x1b]7;file://host/work\x07\x1b[?2004;1004;1002;1006h\x1b[>5u",
    )
    .unwrap();
    let initial = term.snapshot().unwrap();
    for mode in [2004, 1004, 1002, 1006] {
        assert!(initial.input.modes[&mode]);
    }
    assert_eq!(initial.input.kitty_flags, 5);
    assert_eq!(initial.directory, b"file://host/work");
    term.write(b"\x1b[?1049h\x1b[HALT").unwrap();
    assert!(term.input_state().unwrap().alternate);
    assert_eq!(term.snapshot().unwrap().cells[0].text, "A");
    term.write(b"\x1b[?1049l").unwrap();
    assert!(!term.input_state().unwrap().alternate);
    assert_eq!(term.snapshot().unwrap().cells[0].text, "n");
    term.write(b"\x1bc").unwrap();
    let reset = term.snapshot().unwrap();
    assert!(reset.title.is_empty() && reset.directory.is_empty());
    assert!(!reset.input.modes[&2004]);
    assert_eq!(reset.input.kitty_flags, 0);
    assert!(term.take_effects().contains(&Effect::Reset));
    assert!(matches!(
        term.write(b"\x1b[?40h\x1b[?3h"),
        Err(Error::Geometry {
            owned: (12, 3),
            native: (132, 3)
        })
    ));
    assert!(matches!(term.snapshot(), Err(Error::Geometry { .. })));
    term.resize(12, 3, 0, 0).unwrap();
    assert!(term.snapshot().is_ok());
    assert!(Terminal::new(Options::new(0, 3)).is_err());
    assert!(term.resize(65535, 65535, 0, 0).is_err());
}

#[test]
fn palette_defaults_damage_and_inherited_reset() {
    let mut options = Options::new(8, 2);
    options.indexed_entries = 256;
    options.profile.foreground = Some(Rgb(0xd0, 0xc0, 0xb0));
    options.profile.background = Some(Rgb(0x20, 0x30, 0x40));
    options.profile.palette.insert(1, Rgb(0x12, 0x34, 0x56));
    options.profile.modes.insert(1, true);
    options.profile.cursor_style = Some(2);
    options.profile.cursor_blink = Some(true);
    options.profile.scheme = Some(0);
    let mut term = Terminal::new(options).unwrap();
    term.write(b"\x1b[31mA\x1b[0m\x1b[2;1H\x1b[41m\x1b[K\x1b[0m")
        .unwrap();
    let initial = term.snapshot().unwrap();
    assert_eq!(initial.cells[0].style.foreground, Color::Palette(1));
    assert_eq!(initial.cells[8].style.background, Color::Palette(1));
    assert_eq!(initial.cells[1].style.foreground, Color::Default);
    term.clear_damage();
    term.write(b"\x1b]4;1;#abcdef\x07\x1b]10;#111111\x07")
        .unwrap();
    let changed = term.snapshot().unwrap();
    assert_eq!(
        changed.cells[0].style.foreground,
        Color::Rgb(Rgb(0xab, 0xcd, 0xef))
    );
    assert_eq!(
        changed.cells[1].style.foreground,
        Color::Rgb(Rgb(0x11, 0x11, 0x11))
    );
    assert!(term.dirty_rows().iter().all(|d| *d));
    term.write(b"\x1bc\x1b]110\x07\x1b]111\x07\x1b]104;1\x07")
        .unwrap();
    let reset = term.snapshot().unwrap();
    assert_eq!(reset.colors.foreground, initial.colors.foreground);
    assert_eq!(reset.colors.background, initial.colors.background);
    assert_eq!(reset.colors.palette[1], initial.colors.palette[1]);
    assert!(reset.input.modes[&1] && reset.cursor.blinking);
    assert_eq!(reset.cursor.style, 2);
    term.write(b"\x1b[?996n").unwrap();
    assert!(
        term.take_effects()
            .contains(&Effect::Reply(b"\x1b[?997;2n".to_vec()))
    );
}

#[test]
fn erased_cells_and_palette_queries_follow_native_colors() {
    for erase in ["\x1b[K", "\x1b[2K", "\x1b[J", "\x1b[2J"] {
        let mut term = Terminal::new(Options::new(8, 2)).unwrap();
        term.write(format!("\x1b[48;2;10;20;30m{erase}\x1b[2;1H> prompt\x1b[0m").as_bytes())
            .unwrap();
        assert!(
            term.snapshot().unwrap().cells[..8]
                .iter()
                .all(|c| c.text == " " && c.style.background == Color::Rgb(Rgb(10, 20, 30)))
        );
        term.write(b"\x1b]10;#123456\x07\x1b]11;#abcdef\x07\x1b]10;?\x1b\\\x1b]11;?\x1b\\")
            .unwrap();
        let effects = term.take_effects();
        assert!(effects.contains(&Effect::Reply(b"\x1b]10;rgb:1212/3434/5656\x1b\\".to_vec())));
        assert!(effects.contains(&Effect::Reply(b"\x1b]11;rgb:abab/cdcd/efef\x1b\\".to_vec())));
    }
}

#[test]
fn native_mouse_formats_tracking_and_keyboard_probe() {
    let mut term = Terminal::new(Options::new(12, 3)).unwrap();
    assert_eq!(term.input_state().unwrap().mouse_tracking_mode, 0);
    let mut event = Mouse {
        action: 0,
        button: Some(1),
        modifiers: 0,
        x: 2.,
        y: 1.,
    };
    term.write(b"\x1b[?1002;1006h").unwrap();
    assert_eq!(term.encode_mouse(event, false).unwrap(), b"\x1b[<0;3;2M");
    event.action = 2;
    assert_eq!(term.encode_mouse(event, true).unwrap(), b"\x1b[<32;3;2M");
    event.button = None;
    assert!(term.encode_mouse(event, false).unwrap().is_empty());
    term.write(b"\x1b[?1016h").unwrap();
    assert!(term.encode_mouse(event, false).is_err());
    term.resize(12, 3, 9, 18).unwrap();
    event = Mouse {
        action: 0,
        button: Some(1),
        modifiers: 0,
        x: 20.,
        y: 30.,
    };
    assert_eq!(term.encode_mouse(event, false).unwrap(), b"\x1b[<0;20;30M");
    event.button = Some(10);
    assert!(term.encode_mouse(event, false).is_err());
    term.write(b"\x1b[>4;2m").unwrap();
    assert!(term.input_state().unwrap().modify_other_keys_2);
    term.write(b"\x1b[>4;0m").unwrap();
    assert!(!term.input_state().unwrap().modify_other_keys_2);
}

#[test]
fn clipboard_handlers_cannot_mutate_owned_observations_and_panics_are_errors() {
    let mut options = Options::new(8, 2);
    options.clipboard_write = Some(Box::new(|mut request| {
        request.contents[0].data[0] = b'X';
        tui_frame::terminal::ClipboardWriteReply {
            result: 0,
            remember: false,
        }
    }));
    options.clipboard_read = Some(Box::new(|_| tui_frame::terminal::ClipboardReadReply {
        result: 0,
        contents: vec![tui_frame::terminal::ClipboardContent {
            mime: b"text/plain".to_vec(),
            data: b"world".to_vec(),
        }],
        available: Vec::new(),
        remember: false,
    }));
    let mut term = Terminal::new(options).unwrap();
    term.write(b"\x1b]52;c;aGVsbG8=\x1b\\\x1b]52;c;?\x1b\\")
        .unwrap();
    let effects = term.take_effects();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e,Effect::ClipboardWrite(r) if r.contents[0].data==b"hello"))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e,Effect::Reply(b) if b.windows(8).any(|s|s==b"d29ybGQ=")))
    );
    let mut options = Options::new(8, 2);
    options.clipboard_write = Some(Box::new(|_| panic!("handler failed")));
    let mut term = Terminal::new(options).unwrap();
    assert!(
        matches!(term.write(b"\x1b]52;c;eA==\x1b\\"),Err(Error::CallbackPanic(s)) if s=="handler failed")
    );
    term.resize(8, 2, 0, 0).unwrap();
}

#[test]
fn unsupported_graphics_and_bounded_unknown_controls() {
    let mut options = Options::new(8, 2);
    options.unknown_max_bytes = 8;
    options.scrollback_max_lines = Some(20);
    options.scrollback_max_bytes = Some(1 << 20);
    let mut term = Terminal::new(options).unwrap();
    term.write(b"\x1b]9999;abcdefghijklmnop\x07\x1b_Ga=q,i=42,s=1,v=1,f=24;AAAA\x1b\\")
        .unwrap();
    let effects = term.take_effects();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e,Effect::Unknown {truncated:true,content,..} if content.len()<=8))
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e,Effect::Reply(b) if b.windows(2).any(|s|s==b"OK")))
    );
    let state = term.snapshot().unwrap();
    assert_eq!(state.scrollback_max_lines, Some(20));
    assert_eq!(state.scrollback_max_bytes, Some(1 << 20));
    assert!(state.vt_ground && !state.vt_processing_error && state.total_rows >= 2);
}
