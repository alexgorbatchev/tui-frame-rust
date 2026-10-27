use super::*;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
fn with_console(test: impl FnOnce(&mut Console<'_>)) {
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
    let (_master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut console = Console::new(slave.as_fd(), slave.as_fd(), true).unwrap();
    test(&mut console);
}
fn packet(bytes: &[u8]) -> crate::input::Packet {
    crate::input::Packet {
        raw: bytes.to_vec(),
        event: crate::input::Event::Unknown,
        paste: false,
    }
}
#[test]
fn inherited_preferences_match_solicited_controls_and_reject_stale_or_malformed_input() {
    with_console(|console| {
        assert!(!console.consume_reply(&packet(b"\x1b[?1049;2$y")));
        console.queries();
        for invalid in [
            b"\x1b[?1049;5$y".as_slice(),
            b"\x1b[?1049;x$y",
            b"\x1b[?99999;2$y",
            b"\x1b[?4;256u",
            b"\x1b[>4;3m",
            b"\x1b]4;1;not-a-color\x07",
            b"\x1bP1$r7 q\x1b\\",
            b"\x1b[?997;3n",
        ] {
            assert!(!console.consume_reply(&packet(invalid)), "{invalid:?}");
        }
        for reply in [
            b"\x1b[?1049;2$y".as_slice(),
            b"\x1b[4;1$y",
            b"\x1b[?7;3$y",
            b"\x1b[?5u",
            b"\x1b[>4;2m",
            b"\x1b[6;20;10t",
            b"\x1b[?1;2c",
            b"\x1b[?997;2n",
            b"\x1b]10;rgb:11/22/33\x07",
            b"\x1b]11;#445566\x1b\\",
            b"\x1b]12;#778899\x07",
            b"\x1b]4;1;#aabbcc;2;#112233\x07",
            b"\x1bP1$r3 q\x1b\\",
        ] {
            assert!(console.consume_reply(&packet(reply)), "{reply:?}");
        }
        assert!(console.profile.modes[&0x8004]);
        assert!(console.grapheme_width() == console.switchable(2027));
        assert_eq!(console.profile.kitty_flags, Some(5));
        assert_eq!(console.profile.modify_other_keys, Some(2));
        assert_eq!(console.profile.foreground, Some(Rgb(17, 34, 51)));
        assert_eq!(console.profile.palette[&1], Rgb(170, 187, 204));
        assert_eq!((console.cell_width, console.cell_height), (10, 20));
        assert_eq!(console.profile.scheme, Some(0));
        assert!(!console.consume_reply(&packet(b"\x1b[?1;2c")));
        assert!(!console.consume_reply(&packet(b"\x1b[?5u")));
        console.preferences_closed = true;
        assert!(console.consume_reply(&packet(b"\x1b]4;3;#ffffff\x07")));
        assert!(!console.profile.palette.contains_key(&3));
        assert!(console.consume_reply(&packet(b"\x1b[6;0;0t")));
        assert_eq!((console.cell_width, console.cell_height), (10, 20));
        assert!(console.consume_reply(&packet(b"\x1b[6;30;15t")));
        assert_eq!((console.cell_width, console.cell_height), (15, 30));
        let mut paste = packet(b"\x1b[6;20;10t");
        paste.paste = true;
        assert!(!console.consume_reply(&paste));
    });
}
#[test]
fn native_cursor_styles_and_colors_restore_to_entry_appearance() {
    with_console(|console| {
        console.queries();
        assert!(console.consume_reply(&packet(b"\x1bP1$r2 q\x1b\\")));
        assert!(console.consume_reply(&packet(b"\x1b]12;#010203\x07")));
        let mut terminal = terminal::Terminal::new(Options::new(8, 3)).unwrap();
        for (command, expected) in [
            (b"\x1b[6 q".as_slice(), b"\x1b[6 q".as_slice()),
            (b"\x1b[4 q", b"\x1b[4 q"),
            (b"\x1b[1 q", b"\x1b[1 q"),
        ] {
            terminal.write(command).unwrap();
            console.buffer.clear();
            console.sync_cursor(&terminal.snapshot().unwrap());
            assert!(
                console
                    .buffer
                    .windows(expected.len())
                    .any(|v| v == expected)
            );
        }
        terminal.write(b"\x1b[2 q\x1b]12;#010203\x07").unwrap();
        console.buffer.clear();
        console.sync_cursor(&terminal.snapshot().unwrap());
        assert!(console.buffer.starts_with(b"\x1b[0 q"));
        terminal.write(b"\x1b]12;#112233\x07").unwrap();
        console.buffer.clear();
        console.sync_cursor(&terminal.snapshot().unwrap());
        assert!(console.buffer.windows(8).any(|v| v == b"#112233\x07"));
        terminal.write(b"\x1b]12;#010203\x07").unwrap();
        console.buffer.clear();
        console.sync_cursor(&terminal.snapshot().unwrap());
        assert_eq!(console.buffer, b"\x1b]112\x07");
    });
}
#[test]
fn exclusive_mouse_protocols_permanent_modes_and_keyboard_protocols_are_mirrored() {
    with_console(|console| {
        console.queries();
        for &mode in MODES {
            assert!(console.consume_reply(&packet(format!("\x1b[?{mode};2$y").as_bytes())));
        }
        assert!(console.consume_reply(&packet(b"\x1b[?0u")));
        assert!(console.consume_reply(&packet(b"\x1b[>4;1m")));
        console.enter().unwrap();
        let mut terminal = terminal::Terminal::new(Options::new(8, 3)).unwrap();
        terminal
            .write(b"\x1b[?1003;1016;1004h\x1b[>5u\x1b[>4;2m")
            .unwrap();
        console.cell_width = 10;
        console.cell_height = 20;
        console
            .sync_input(&terminal.input_state().unwrap())
            .unwrap();
        assert_eq!(console.applied.get(&1003), Some(&true));
        assert_eq!(console.applied.get(&1016), Some(&true));
        assert_eq!(console.kitty_flags, 5);
        assert_eq!(console.modify_level, 2);
        terminal.write(b"\x1b[?1016l").unwrap();
        console
            .sync_input(&terminal.input_state().unwrap())
            .unwrap();
        assert_eq!(console.applied.get(&1016), Some(&false));
        assert_eq!(console.applied.get(&1006), Some(&true));
        console.entry.insert(1016, 3);
        console.cell_width = 0;
        console.cell_height = 0;
        console
            .sync_input(&terminal.input_state().unwrap())
            .unwrap();
        assert_eq!(console.applied.get(&1003), Some(&false));
        console.entry.insert(7, 3);
        assert!(console.grapheme_width() == console.switchable(2027));
        let options = console.child_options(8, 3);
        assert_eq!(options.terminfo_name, "xterm-256color");
        console.set_color_profile(ColorProfile::Ansi256);
        assert_eq!(console.child_options(8, 3).indexed_entries, 256);
    });
}
#[test]
fn environment_color_tiers_and_nonempty_no_color_follow_terminal_contract() {
    for (variables, expected) in [
        (vec![("TERM", "xterm")], ColorProfile::Ansi),
        (vec![("TERM", "xterm-256color")], ColorProfile::Ansi256),
        (
            vec![("TERM", "xterm"), ("COLORTERM", "truecolor")],
            ColorProfile::TrueColor,
        ),
        (
            vec![("TERM", "screen"), ("COLORTERM", "truecolor")],
            ColorProfile::Ansi256,
        ),
        (vec![("TERM", "xterm-ghostty")], ColorProfile::TrueColor),
        (vec![("TERM", "dumb")], ColorProfile::None),
        (
            vec![("TERM", "dumb"), ("CLICOLOR_FORCE", "1")],
            ColorProfile::Ansi,
        ),
        (
            vec![("TERM", "xterm"), ("NO_COLOR", "false")],
            ColorProfile::None,
        ),
        (
            vec![("TERM", "xterm"), ("NO_COLOR", "")],
            ColorProfile::Ansi,
        ),
    ] {
        let environment = variables
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(environment_profile(&environment), expected);
    }
}

#[test]
fn native_color_probe_reads_capabilities_and_reaps_a_timed_out_process() {
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "printf 'RGB: true\\n'"]);
    assert!(bounded_truecolor_probe(&mut command, Duration::from_secs(1)).unwrap());
    let mut command = std::process::Command::new("/bin/sleep");
    command.arg("2");
    let start = Instant::now();
    assert!(!bounded_truecolor_probe(&mut command, Duration::from_millis(20)).unwrap());
    assert!(start.elapsed() < Duration::from_secs(1));
    let mut command = std::process::Command::new("/bin/false");
    assert!(!bounded_truecolor_probe(&mut command, Duration::from_secs(1)).unwrap());
    // Exercise short-lived producers whose final write races the reader.
    for _ in 0..32 {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf 'Tc: true\\n'"]);
        assert!(bounded_truecolor_probe(&mut command, Duration::from_secs(1)).unwrap());
    }
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "dd if=/dev/zero bs=65537 count=1 2>/dev/null"]);
    assert!(!bounded_truecolor_probe(&mut command, Duration::from_secs(1)).unwrap());
}
