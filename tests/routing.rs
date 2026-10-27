use ratatui::layout::Rect;
use tui_frame::{
    input::{Event, Framer, KeyEventKind},
    routing::{Capture, HostProfile, Origin, Router},
    terminal::{Options, Terminal},
};

fn packet(raw: &[u8]) -> tui_frame::input::Packet {
    let mut f = Framer::new();
    let mut out = f.feed(raw).unwrap();
    out.extend(f.flush());
    out.remove(0)
}
fn terminal(modes: &[u8]) -> Terminal {
    let mut t = Terminal::new(Options::new(80, 24)).unwrap();
    t.write(modes).unwrap();
    t
}

#[test]
fn capture_keeps_gesture_ownership_and_raw_bytes() {
    let mut t = terminal(b"");
    let mut r = Router::new();
    r.host.kitty_flags = 2;
    let viewport = Rect::new(0, 0, 80, 24);
    let press = packet(b"\x1b[1080::98;5u");
    let release = packet(b"\x1b[1080;5:3u");
    let mut seen = Vec::new();
    let mut capture = |key: &tui_frame::input::Key| {
        seen.push(key.raw.clone());
        if key.event.kind == KeyEventKind::Press {
            Capture::Consume
        } else {
            Capture::Pass
        }
    };
    assert!(
        r.route(&press, &mut t, viewport, Some(&mut capture))
            .unwrap()
            .captured
    );
    assert!(
        r.route(&release, &mut t, viewport, Some(&mut capture))
            .unwrap()
            .captured
    );
    assert_eq!(seen, [press.raw.clone(), release.raw.clone()]);
    assert_eq!(
        r.route(&release, &mut t, viewport, None).unwrap().bytes,
        release.raw
    );
    assert_eq!(
        r.route(&packet(b"\x1b]unknown\x07"), &mut t, viewport, None)
            .unwrap()
            .bytes,
        b"\x1b]unknown\x07"
    );
}

#[test]
fn paste_markers_latch_and_focus_follows_child_mode() {
    let mut t = terminal(b"\x1b[?2004h\x1b[?1004h");
    let mut r = Router::new();
    let v = Rect::new(0, 0, 80, 24);
    let mut f = Framer::new();
    let ps = f.feed(b"\x1b[200~\xff\x1b[201~").unwrap();
    assert_eq!(r.route(&ps[0], &mut t, v, None).unwrap().bytes, ps[0].raw);
    t.write(b"\x1b[?2004l").unwrap();
    assert_eq!(r.route(&ps[1], &mut t, v, None).unwrap().bytes, [0xff]);
    assert_eq!(r.route(&ps[2], &mut t, v, None).unwrap().bytes, ps[2].raw);
    assert!(
        !r.route(&packet(b"\x1b[I"), &mut t, v, None)
            .unwrap()
            .bytes
            .is_empty()
    );
    t.write(b"\x1b[?1004l").unwrap();
    assert!(
        r.route(&packet(b"\x1b[O"), &mut t, v, None)
            .unwrap()
            .bytes
            .is_empty()
    );
}

#[test]
fn native_mouse_translation_ownership_and_errors() {
    let mut t = terminal(b"\x1b[?1003h\x1b[?1006h");
    let mut r = Router::new();
    let v = Rect::new(2, 3, 70, 20);
    assert_eq!(
        r.route(&packet(b"\x1b[<0;4;5M"), &mut t, v, None)
            .unwrap()
            .bytes,
        b"\x1b[<0;2;2M"
    );
    let release = packet(b"\x1b[M#$$");
    let out = r.route(&release, &mut t, v, None).unwrap();
    assert_eq!(out.bytes, b"\x1b[<0;2;1m");
    assert!(out.bytes.len() <= Router::max_output_size(&release));
    let out = r.route(&packet(b"\x1b[<0;4;5m"), &mut t, v, None).unwrap();
    assert_eq!(out.origin, Origin::UnownedGesture);
    let out = r
        .route(&packet(b"\x1b[<131;4;5M"), &mut t, v, None)
        .unwrap();
    assert_eq!(out.origin, Origin::Unroutable);
    assert!(out.error.is_some());
    let out = r.route(&packet(b"\x1b[<0;1;1M"), &mut t, v, None).unwrap();
    assert_eq!(out.origin, Origin::OutsideChild);
    r.host = HostProfile {
        mouse_pixels: true,
        ..Default::default()
    };
    assert_eq!(
        r.route(&packet(b"\x1b[<0;4;5M"), &mut t, v, None)
            .unwrap()
            .origin,
        Origin::UnmeasuredPixels
    );
    assert!(matches!(packet(b"\x1b[<0;4;5M").event, Event::Mouse(_)));
}

#[test]
fn gesture_protocol_transitions_repeats_and_physical_keys() {
    let mut t = terminal(b"\x1b[>3u");
    let v = Rect::new(0, 0, 80, 24);
    for (press, release, captured) in [
        ("\x1b[1080;5u", "\x1b[1080::98;5:3u", true),
        ("\x1b[66;48;1080;1;8;1_", "\x1b[66;48;0;0;8;1_", true),
        ("\x1b[120::98;5u", "\x1b[98::110;5:3u", false),
        ("A", "\x1b[97;1:3u", true),
        ("\x1b[17~", "\x1b[17;1:3~", true),
    ] {
        let mut r = Router::new();
        r.host.kitty_flags = 2;
        let mut handler = |key: &tui_frame::input::Key| {
            if key.event.kind == KeyEventKind::Press {
                Capture::Consume
            } else {
                Capture::Pass
            }
        };
        assert!(
            r.route(&packet(press.as_bytes()), &mut t, v, Some(&mut handler))
                .unwrap()
                .captured
        );
        assert_eq!(
            r.route(&packet(release.as_bytes()), &mut t, v, Some(&mut handler))
                .unwrap()
                .captured,
            captured,
            "{press:?} {release:?}"
        );
    }
    let mut r = Router::new();
    r.host.kitty_flags = 2;
    let mut pass = |_: &tui_frame::input::Key| Capture::Pass;
    let mut consume = |_: &tui_frame::input::Key| Capture::Consume;
    r.route(&packet(b"\x1b[113u"), &mut t, v, Some(&mut pass))
        .unwrap();
    for raw in [b"\x1b[113;5:2u".as_slice(), b"\x1b[113;1:3u"] {
        assert!(
            !r.route(&packet(raw), &mut t, v, Some(&mut consume))
                .unwrap()
                .captured
        );
    }
    r.route(&packet(b"q"), &mut t, v, Some(&mut consume))
        .unwrap();
    r.route(&packet(b"q"), &mut t, v, Some(&mut pass)).unwrap();
    assert!(
        !r.route(&packet(b"\x1b[113;1:3u"), &mut t, v, Some(&mut consume))
            .unwrap()
            .captured
    );
    r.route(&packet(b"q"), &mut t, v, Some(&mut consume))
        .unwrap();
    r.host.kitty_flags = 0;
    assert!(
        !r.route(&packet(b"\x1b[113;1:3u"), &mut t, v, Some(&mut pass))
            .unwrap()
            .captured
    );
    r.host.kitty_flags = 2;
    assert!(
        r.route(&packet(b"\x1b[113;1:3u"), &mut t, v, Some(&mut consume))
            .unwrap()
            .captured
    );
    assert_eq!(Router::max_output_size(&packet(b"a")), 1);
}

#[test]
fn mouse_modes_bounds_and_failed_press_do_not_own_drag() {
    let v = Rect::new(2, 3, 2040, 5);
    for (modes, raw) in [
        (b"\x1b[?1000h".as_slice(), b"\x1b[<0;226;8M".as_slice()),
        (b"\x1b[?1000;1005h", b"\x1b[<0;2018;8M"),
        (b"\x1b[?1000;1005h", b"\x1b[<128;6;8M"),
    ] {
        let mut t = Terminal::new(Options::new(2040, 5)).unwrap();
        t.write(modes).unwrap();
        let mut r = Router::new();
        let out = r.route(&packet(raw), &mut t, v, None).unwrap();
        assert_eq!(out.origin, Origin::Unroutable);
        assert!(out.error.is_some());
    }
    let v = Rect::new(2, 3, 70, 20);
    let mut t = terminal(b"\x1b[?1002;1006h");
    let mut r = Router::new();
    for raw in [b"\x1b[<130;6;8M".as_slice(), b"\x1b[<131;6;8M"] {
        assert_eq!(
            r.route(&packet(raw), &mut t, v, None).unwrap().origin,
            Origin::Unroutable
        );
    }
    for raw in [
        b"\x1b[<162;7;8M".as_slice(),
        b"\x1b[<130;7;8m",
        b"\x1b[<163;7;8M",
        b"\x1b[<131;7;8m",
    ] {
        assert_eq!(
            r.route(&packet(raw), &mut t, v, None).unwrap().origin,
            Origin::UnownedGesture
        );
    }
    let p = packet(b"\x1b[<0;6;8M");
    r.route(&p, &mut t, v, None).unwrap();
    r.route(&packet(b"\x1b[<0;1;1m"), &mut t, v, None).unwrap();
    assert_eq!(
        r.route(&packet(b"\x1b[<32;6;8M"), &mut t, v, None)
            .unwrap()
            .origin,
        Origin::UnownedGesture
    );
    t.write(b"\x1b[?1002l").unwrap();
    assert_eq!(
        r.route(&p, &mut t, v, None).unwrap().origin,
        Origin::MouseDisabled
    );
    t.write(b"\x1b[?9h").unwrap();
    r.route(&p, &mut t, v, None).unwrap();
    assert!(
        r.route(&packet(b"\x1b[<0;6;8m"), &mut t, v, None)
            .unwrap()
            .bytes
            .is_empty()
    );
    assert!(r.route(&p, &mut t, Rect::default(), None).is_err());
}

#[test]
fn pixels_preserve_offsets_and_geometry_loss_releases_ownership() {
    let v = Rect::new(2, 3, 8, 5);
    for (mode, expected) in [
        (b"\x1b[?1000;1016h".as_slice(), b"\x1b[<0;41;31M".as_slice()),
        (b"\x1b[?1000;1006h", b"\x1b[<0;5;2M"),
    ] {
        let mut options = Options::new(8, 5);
        options.cell_width_px = 10;
        options.cell_height_px = 20;
        let mut t = Terminal::new(options).unwrap();
        t.write(mode).unwrap();
        let mut r = Router::new();
        r.host = HostProfile {
            mouse_pixels: true,
            cell_width_px: 10,
            cell_height_px: 20,
            ..Default::default()
        };
        assert_eq!(
            r.route(&packet(b"\x1b[<0;61;91M"), &mut t, v, None)
                .unwrap()
                .bytes,
            expected
        );
        r.host.cell_width_px = 0;
        t.resize(8, 5, 0, 0).unwrap();
        assert_eq!(
            r.route(&packet(b"\x1b[<0;61;91m"), &mut t, v, None)
                .unwrap()
                .origin,
            Origin::UnmeasuredPixels
        );
        r.host.cell_width_px = 10;
        t.resize(8, 5, 10, 20).unwrap();
        assert_eq!(
            r.route(&packet(b"\x1b[<32;61;91M"), &mut t, v, None)
                .unwrap()
                .origin,
            Origin::UnownedGesture
        );
        r.host.cell_width_px = 12;
        assert!(
            r.route(&packet(b"\x1b[<0;61;91M"), &mut t, v, None)
                .is_err()
        );
    }
    let mut t = terminal(b"\x1b[?1000;1016h");
    let mut r = Router::new();
    assert_eq!(
        r.route(&packet(b"\x1b[<0;6;8M"), &mut t, v, None)
            .unwrap()
            .origin,
        Origin::Unroutable
    );
}

#[test]
fn anonymous_release_expansion_and_all_native_formats() {
    let v = Rect::new(2, 3, 500, 300);
    for format in ["", "\x1b[?1005h", "\x1b[?1006h", "\x1b[?1015h"] {
        let mut t = Terminal::new(Options::new(500, 300)).unwrap();
        t.write(format!("\x1b[?1003h{format}").as_bytes()).unwrap();
        let mut r = Router::new();
        for cb in [0, 1, 2, 64, 65, 66, 67, 128, 129, 130, 131] {
            let p = packet(format!("\x1b[<{cb};4;5M").as_bytes());
            let out = r.route(&p, &mut t, v, None).unwrap();
            assert!(out.bytes.len() <= Router::max_output_size(&p));
        }
        let p = packet(b"\x1b[M?\xff\xff");
        let out = r.route(&p, &mut t, v, None).unwrap();
        assert!(out.bytes.len() <= Router::max_output_size(&p));
        assert_eq!(
            r.route(&p, &mut t, v, None).unwrap().origin,
            Origin::UnownedGesture
        );
    }
    let mut t = terminal(b"\x1b[?1003;1006h");
    let v = Rect::new(0, 0, 80, 24);
    let mut r = Router::new();
    r.route(&packet(b"\x1b[<3;2;3M"), &mut t, v, None).unwrap();
    assert!(
        !r.route(&packet(b"\x1b[M#\x22\x23"), &mut t, v, None)
            .unwrap()
            .bytes
            .is_empty()
    );
    let mut p = packet(b"\x1b[<0;2;3M");
    let Event::Mouse(ref mut m) = p.event else {
        panic!()
    };
    m.modifiers = tui_frame::input::Modifiers::HYPER;
    assert_eq!(
        r.route(&p, &mut t, v, None).unwrap().origin,
        Origin::Unroutable
    );
    let Event::Mouse(ref mut m) = p.event else {
        panic!()
    };
    m.modifiers = tui_frame::input::Modifiers::SUPER
        | tui_frame::input::Modifiers::CAPS_LOCK
        | tui_frame::input::Modifiers::NUM_LOCK;
    m.x = i64::MIN;
    assert_eq!(
        r.route(&p, &mut t, v, None).unwrap().origin,
        Origin::OutsideChild
    );
    let Event::Mouse(ref mut m) = p.event else {
        panic!()
    };
    m.x = 2;
    assert!(!r.route(&p, &mut t, v, None).unwrap().bytes.is_empty());
}
