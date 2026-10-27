use tui_frame::terminal::{Color, Effect, Options, Rgb, State, Terminal};

fn filled_frame(character: char) -> String {
    let row = character.to_string().repeat(8);
    format!("\x1b[H{row}\r\n{row}\r\n{row}")
}

#[test]
fn superseded_synchronized_frames_convert_only_when_observed() {
    for held in [false, true] {
        let mut terminal = Terminal::new(Options::new(8, 3)).unwrap();
        terminal.write(filled_frame('o').as_bytes()).unwrap();
        let original = terminal.snapshot().unwrap();
        terminal.clear_damage();
        let mut output = String::new();
        for character in ['a', 'b', 'c'] {
            output.push_str("\x1b[?2026h");
            output.push_str(&filled_frame(character));
            if character != 'c' || !held {
                output.push_str("\x1b[?2026l");
            }
        }
        terminal.write(output.as_bytes()).unwrap();
        assert!(terminal.dirty_rows().iter().all(|dirty| !dirty));
        let observed = terminal.snapshot().unwrap();
        assert_eq!(observed.input.held, held);
        let expected = if held { "b" } else { "c" };
        assert!(observed.cells.iter().all(|cell| cell.text == expected));
        assert!(terminal.dirty_rows().iter().all(|dirty| *dirty));
        assert!(original.cells.iter().all(|cell| cell.text == "o"));
        if held {
            terminal.release_hold().unwrap();
            assert!(
                terminal
                    .snapshot()
                    .unwrap()
                    .cells
                    .iter()
                    .all(|cell| cell.text == "c")
            );
        }
    }
}

#[test]
fn held_visuals_keep_links_completed_in_the_same_write() {
    const LINK: &str = "\x1b]8;;https://example.test/held\x1b\\held\x1b]8;;\x1b\\";
    const REPLACEMENT: &str = "\x1b[H\x1b]8;;https://example.test/next\x1b\\next\x1b]8;;\x1b\\";
    for (before, complete, update, link) in [
        ("", "\x1b[Hheld", "\x1b[Hnext", ""),
        ("", "\x1b[Hheld", REPLACEMENT, ""),
        ("", LINK, REPLACEMENT, "https://example.test/held"),
        (
            LINK,
            "\x1b]10;#123456\x07",
            REPLACEMENT,
            "https://example.test/held",
        ),
    ] {
        let mut terminal = Terminal::new(Options::new(8, 2)).unwrap();
        terminal.write(before.as_bytes()).unwrap();
        terminal.snapshot().unwrap();
        terminal
            .write(format!("{complete}\x1b[?2026h{update}").as_bytes())
            .unwrap();
        let held = terminal.snapshot().unwrap();
        assert!(held.input.held);
        assert_eq!(
            held.cells[..4]
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "held"
        );
        assert!(
            held.cells[..4]
                .iter()
                .all(|cell| cell.hyperlink == link.as_bytes())
        );
        terminal.release_hold().unwrap();
        let released = terminal.snapshot().unwrap();
        assert!(!released.input.held);
        assert_eq!(released.cells[0].text, "n");
        assert_eq!(held.cells[0].text, "h");
    }
}

#[test]
fn active_mouse_tracking_is_distinct_from_retained_mode_bits() {
    for (controls, mode, any_bit) in [
        ("", 0, false),
        ("\x1b[?9h", 1, true),
        ("\x1b[?1000h", 2, true),
        ("\x1b[?1002h", 3, true),
        ("\x1b[?1003h", 4, true),
        ("\x1b[?1000h\x1b[?1002h\x1b[?1002l", 0, true),
        ("\x1b[?1000h\x1b[?1000l", 0, false),
        ("\x1b[?1003h\x1b[?1000h", 2, true),
        ("\x1b[?1003h\x1b[?1000h\x1b[?1003l", 0, true),
        ("\x1b[?1016h\x1b[?1002h", 3, true),
        ("\x1b[?1002s\x1b[?1000h\x1b[?1002h\x1b[?1002r", 0, true),
        ("\x1b[?1003h\x1bc", 0, false),
    ] {
        let mut terminal = Terminal::new(Options::new(8, 2)).unwrap();
        terminal.write(controls.as_bytes()).unwrap();
        terminal.take_effects();
        let state = terminal.snapshot().unwrap();
        assert_eq!(
            (state.input.mouse_tracking_mode, state.input.mouse_tracking),
            (mode, any_bit),
            "{controls:?}"
        );
        assert!(
            terminal.take_effects().is_empty(),
            "observation emitted PTY effects"
        );
    }
}

#[test]
fn keyboard_format_observation_preserves_parser_state_and_emits_no_replies() {
    let mut terminal = Terminal::new(Options::new(8, 2)).unwrap();
    for (fragments, expected) in [
        (vec![], false),
        (vec!["\x1b[>4;", "2m"], true),
        (vec!["\x1b[>31u"], true),
        (vec!["\x1b[4;0m"], true),
        (vec!["\x1b[>4;0m"], false),
        (vec!["\x1b[>4;1m"], false),
        (vec!["\x1b[>4;2m"], true),
        (vec!["\x1b[>4;0;9m"], true),
        (vec!["\x1b[>m"], false),
        (vec!["\x1b[>4;2m"], true),
        (vec!["\x1bc"], false),
    ] {
        for fragment in &fragments {
            terminal.write(fragment.as_bytes()).unwrap();
        }
        terminal.take_effects();
        assert_eq!(
            terminal.snapshot().unwrap().input.modify_other_keys_2,
            expected,
            "{fragments:?}"
        );
        assert!(terminal.take_effects().is_empty());
    }
}

fn check_palette(state: &State, one: Color, high: Color) {
    assert_eq!(state.cells[0].style.foreground, one);
    assert_eq!(state.cells[1].style.foreground, high);
    assert_eq!(state.cells[2].style.background, one);
    assert_eq!(state.cells[3].style.underline_color, one);
    assert_eq!(
        state.cells[4].style.foreground,
        Color::Rgb(state.colors.palette[2])
    );
    assert!(
        state.cells[8..16]
            .iter()
            .all(|cell| cell.style.background == one)
    );
}

#[test]
fn inherited_palette_indices_follow_the_outer_color_capacity() {
    const PALETTE_CELLS: &[u8] = b"\x1b[31mA\x1b[0;38;5;200mB\x1b[0;41mC\x1b[0;4;58;5;1mD\x1b[0;32mE\x1b[0m\x1b[2;1H\x1b[41m\x1b[K\x1b[0m";
    for capacity in [0, 16, 256] {
        let mut options = Options::new(8, 2);
        options.indexed_entries = capacity;
        options.profile.palette.insert(1, Rgb(0xdc, 0x32, 0x2f));
        options.profile.palette.insert(200, Rgb(0x65, 0x43, 0x21));
        let mut terminal = Terminal::new(options).unwrap();
        terminal.write(PALETTE_CELLS).unwrap();
        let one = if capacity > 1 {
            Color::Palette(1)
        } else {
            Color::Rgb(Rgb(0xdc, 0x32, 0x2f))
        };
        let high = if capacity > 200 {
            Color::Palette(200)
        } else {
            Color::Rgb(Rgb(0x65, 0x43, 0x21))
        };
        check_palette(&terminal.snapshot().unwrap(), one, high);
        terminal.write(b"\x1b]4;1;#123456;200;#abcdef\x07").unwrap();
        check_palette(
            &terminal.snapshot().unwrap(),
            Color::Rgb(Rgb(0x12, 0x34, 0x56)),
            Color::Rgb(Rgb(0xab, 0xcd, 0xef)),
        );
        terminal.write(b"\x1b]104\x07").unwrap();
        check_palette(&terminal.snapshot().unwrap(), one, high);
    }
}

#[test]
fn cloned_snapshots_and_effects_keep_independent_native_payloads() {
    let mut options = Options::new(8, 2);
    options.scrollback_max_bytes = Some(65536);
    options.scrollback_max_lines = Some(20);
    let mut terminal = Terminal::new(options).unwrap();
    terminal.write(b"\x1b[1mx\x1b[0m\x1b[?2004h\x1b]2;owned\x07\x1b[6n\x1b]52;c;aGVsbG8=\x1b\\\x1b]9999;unknown\x07").unwrap();
    let original = terminal.snapshot().unwrap();
    let mut cloned = original.clone();
    let style_index = original.cells[0].style_index;
    cloned.cells[0].text.clear();
    cloned.native_styles[style_index].bold = false;
    cloned.input.modes.insert(2004, false);
    cloned.input.mode_errors.insert(2004, "changed".into());
    cloned.title.clear();
    cloned.scrollback_max_bytes = Some(1);
    cloned.scrollback_max_lines = Some(1);
    assert_eq!(original.cells[0].text, "x");
    assert!(original.native_styles[style_index].bold);
    assert!(original.input.modes[&2004]);
    assert!(!original.input.mode_errors.contains_key(&2004));
    assert_eq!(original.title, b"owned");
    assert_eq!(
        (original.scrollback_max_bytes, original.scrollback_max_lines),
        (Some(65536), Some(20))
    );
    let observed = terminal.take_effects();
    let mut copied = observed.clone();
    for effect in &mut copied {
        match effect {
            Effect::Reply(bytes)
            | Effect::Title(bytes)
            | Effect::Unknown { content: bytes, .. } => bytes.fill(b'x'),
            Effect::ClipboardWrite(request) => request.contents[0].data.fill(b'x'),
            _ => {}
        }
    }
    assert!(
        observed
            .iter()
            .any(|effect| matches!(effect, Effect::Reply(bytes) if bytes.starts_with(b"\x1b[")))
    );
    assert!(
        observed
            .iter()
            .any(|effect| matches!(effect, Effect::Title(bytes) if bytes == b"owned"))
    );
    assert!(observed.iter().any(|effect| matches!(effect, Effect::ClipboardWrite(request) if request.contents[0].data == b"hello")));
    assert!(observed.iter().any(
        |effect| matches!(effect, Effect::Unknown {content, ..} if content == b"9999;unknown")
    ));
    assert!(terminal.take_effects().is_empty());
}

#[test]
fn incremental_rows_retain_damage_and_rebase_owned_style_indices() {
    let mut terminal = Terminal::new(Options::new(8, 3)).unwrap();
    terminal
        .write(b"\x1b[1mA\x1b[0;3mB\x1b[0m\x1b[2;1H\x1b[4mC\x1b[0m\x1b[3;1H\x1b[9mD\x1b[0m\x1b[H")
        .unwrap();
    let first = terminal.snapshot().unwrap();
    terminal.clear_damage();
    terminal.write(b"\x1b[2K\x1b[Hplain\x1b[H").unwrap();
    let second = terminal.snapshot().unwrap();
    assert_eq!(terminal.dirty_rows(), &[true, false, false]);
    assert_eq!(second.cells[0].text, "p");
    assert!(second.native_styles[second.cells[8].style_index].underline > 0);
    assert!(second.native_styles[second.cells[16].style_index].strikethrough);
    assert!(first.native_styles[first.cells[0].style_index].bold);
    assert!(first.native_styles[first.cells[1].style_index].italic);
    terminal.snapshot().unwrap();
    assert_eq!(terminal.dirty_rows(), &[true, false, false]);
    terminal.clear_damage();
    terminal.snapshot().unwrap();
    assert_eq!(terminal.dirty_rows(), &[false, false, false]);
}

#[test]
fn clean_linked_rows_survive_a_deferred_unlinked_update() {
    let mut terminal = Terminal::new(Options::new(8, 3)).unwrap();
    terminal
        .write(b"\x1b]8;;https://example.test/stable\x1b\\link\x1b]8;;\x1b\\\x1b[2;1Hold\x1b[2;1H")
        .unwrap();
    terminal.snapshot().unwrap();
    terminal.clear_damage();
    terminal.write(b"done\x1b[2;1H\x1b[?2026hnext").unwrap();
    assert_eq!(terminal.dirty_rows(), &[false, false, false]);
    let held = terminal.snapshot().unwrap();
    assert_eq!(terminal.dirty_rows(), &[false, true, false]);
    assert_eq!(held.cells[0].hyperlink, b"https://example.test/stable");
    assert_eq!(held.cells[8].text, "d");
    terminal.release_hold().unwrap();
    let released = terminal.snapshot().unwrap();
    assert_eq!(released.cells[0].hyperlink, held.cells[0].hyperlink);
    assert_eq!(released.cells[8].text, "n");
}

#[test]
fn resize_ends_pending_hold_and_rebuilds_native_geometry() {
    let mut terminal = Terminal::new(Options::new(8, 3)).unwrap();
    terminal.write(b"old\x1b[?2026h\rnew").unwrap();
    terminal.resize(10, 4, 9, 18).unwrap();
    let resized = terminal.snapshot().unwrap();
    assert!(!resized.input.held);
    assert_eq!(resized.cells.len(), 40);
    assert_eq!(resized.cells[0].text, "n");
    assert!(terminal.dirty_rows().iter().all(|dirty| *dirty));
    terminal.clear_damage();
    terminal.write(b"\x1b[?2026h\rnext").unwrap();
    assert_eq!(terminal.snapshot().unwrap().cells[0].text, "n");
    terminal.release_hold().unwrap();
    assert_eq!(terminal.snapshot().unwrap().cells[1].text, "e");
}

#[test]
fn native_shell_notifications_and_queries_remain_ordered_owned_effects() {
    let mut options = Options::new(8, 3);
    options.version = "frame-regression-1".into();
    options.terminfo_name = "xterm-256color".into();
    options.profile.cursor = Some(Rgb(1, 2, 3));
    options.profile.kitty_flags = Some(5);
    options.profile.modify_other_keys = Some(2);
    let mut terminal = Terminal::new(options).unwrap();
    let inherited = terminal.input_state().unwrap();
    assert_eq!(inherited.kitty_flags, 5);
    assert!(inherited.modify_other_keys_2);
    assert_eq!(
        terminal.snapshot().unwrap().colors.cursor,
        Some(Rgb(1, 2, 3))
    );
    terminal.take_effects();
    terminal.write(b"\x1b[>q\x1b]9;4;1;42\x07\x1b]777;notify;Build;Finished\x07\x1b]133;P;k=r\x07\x1b]133;B\x07\x1b]133;C;cmdline_url=ls%20-la\x07\x1b]133;D;-1;err=boom\x07\x1b]133;D\x07\x1b_invalid\x1b\\").unwrap();
    let effects = terminal.take_effects();
    assert!(
        matches!(&effects[0], Effect::Reply(bytes) if bytes.windows(18).any(|window| window == b"frame-regression-1"))
    );
    assert_eq!(
        effects[1],
        Effect::Progress {
            state: 1,
            progress: 42
        }
    );
    assert_eq!(
        effects[2],
        Effect::Notification {
            title: b"Build".to_vec(),
            body: b"Finished".to_vec()
        }
    );
    assert!(matches!(
        &effects[3],
        Effect::SemanticPrompt {
            kind: 1,
            prompt_kind: 1,
            ..
        }
    ));
    assert!(matches!(
        &effects[4],
        Effect::SemanticPrompt { kind: 2, .. }
    ));
    assert!(
        matches!(&effects[5], Effect::SemanticPrompt {kind: 3, command, ..} if command == b"ls -la")
    );
    assert!(
        matches!(&effects[6], Effect::SemanticPrompt {kind: 4, exit_code: Some(-1), error, ..} if error == b"boom")
    );
    assert!(matches!(
        &effects[7],
        Effect::SemanticPrompt {
            kind: 4,
            exit_code: None,
            ..
        }
    ));
    assert!(
        matches!(&effects[8], Effect::Unknown {content, terminator: None, ..} if content == b"invalid")
    );
    terminal.write(b"\x1b]777;notify;Next;Other\x07").unwrap();
    assert_eq!(
        effects[2],
        Effect::Notification {
            title: b"Build".to_vec(),
            body: b"Finished".to_vec()
        }
    );
}

#[test]
fn rewriting_identical_text_with_new_sgr_preserves_the_new_native_style() {
    let mut terminal = Terminal::new(Options::new(12, 2)).unwrap();
    terminal.write(b"\x1b[H\x1b[0;1mX").unwrap();
    let first = terminal.snapshot().unwrap();
    terminal.clear_damage();
    terminal.write(b"\x1b[H\x1b[0;1;91;105;58;5;1mX").unwrap();
    let next = terminal.snapshot().unwrap();
    assert_eq!(
        next.native_styles[next.cells[0].style_index].foreground,
        Color::Palette(9)
    );
    assert_eq!(
        next.native_styles[next.cells[0].style_index].background,
        Color::Palette(13)
    );
    assert_eq!(
        next.native_styles[next.cells[0].style_index].underline_color,
        Color::Palette(1)
    );
    assert_eq!(
        first.native_styles[first.cells[0].style_index].foreground,
        Color::Default
    );
    assert_eq!(terminal.dirty_rows(), &[true, false]);
}
