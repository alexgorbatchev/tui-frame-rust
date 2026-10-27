use tui_frame::input::{Event, Framer, KeyCode, KeyEventKind, Modifiers};

fn packets(raw: &[u8]) -> Vec<tui_frame::input::Packet> {
    let mut f = Framer::new();
    let mut out = f.feed(raw).unwrap();
    out.extend(f.flush());
    out
}

#[test]
fn fragmented_input_retains_exact_bytes_and_semantics() {
    for raw in [
        b"\x1b[17~".as_slice(),
        b"\x1bOA",
        b"\x8fP",
        "界".as_bytes(),
        "\x1bé".as_bytes(),
        b"\x1b\x01",
        b"\x1b ",
        b"\x1b[<0;18;5M",
    ] {
        let mut f = Framer::new();
        let mut out = vec![];
        for byte in raw {
            out.extend(f.feed(&[*byte]).unwrap());
        }
        assert_eq!(out.len(), 1, "{raw:?}");
        assert_eq!(out[0].raw, raw);
        assert!(!matches!(out[0].event, Event::Unknown), "{raw:?}");
    }
    let Event::Key(key) = &packets(b"\x1b[17~")[0].event else {
        panic!()
    };
    assert_eq!(key.event.code, KeyCode::Function(6));
    let Event::Mouse(mouse) = &packets(b"\x1b[<0;18;5M")[0].event else {
        panic!()
    };
    assert_eq!((mouse.x, mouse.y), (17, 4));
}

#[test]
fn ambiguity_paste_and_opaque_controls_preserve_source() {
    for raw in [b"\x1b".as_slice(), b"\x1b[", b"\x1bO", b"\x1b]"] {
        let mut f = Framer::new();
        assert!(f.feed(raw).unwrap().is_empty());
        assert!(f.pending());
        assert!(f.needs_deadline());
        let packet = f.flush().unwrap();
        assert_eq!(packet.raw, raw);
        assert!(matches!(packet.event, Event::Key(_)));
        assert!(f.flush().is_none());
    }
    let mut f = Framer::new();
    f.feed(b"\x1b[17").unwrap();
    assert!(!f.needs_deadline());
    assert!(matches!(f.flush().unwrap().event, Event::Unknown));
    let raw = b"\x1b[200~hello\x11\xff\x1b[17~\nworld\x1b[201~";
    let mut f = Framer::new();
    let mut out = vec![];
    for byte in raw {
        out.extend(f.feed(&[*byte]).unwrap());
    }
    assert!(matches!(out[0].event, Event::PasteStart));
    assert!(matches!(out.last().unwrap().event, Event::PasteEnd));
    assert!(
        out[1..out.len() - 1]
            .iter()
            .all(|p| p.paste && matches!(p.event, Event::Unknown))
    );
    assert_eq!(
        out.iter().flat_map(|p| p.raw.clone()).collect::<Vec<_>>(),
        raw
    );
    for raw in [
        b"\x1b]9999;opaque\x1b\\".as_slice(),
        b"\x1bPopaque\x1b\\",
        b"\x9dopaque\x9c",
    ] {
        let mut f = Framer::new();
        let mut out = vec![];
        for byte in raw {
            out.extend(f.feed(&[*byte]).unwrap());
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].raw, raw);
    }
    let mut large = b"\x1b]".to_vec();
    large.extend(vec![b'x'; 1 << 20]);
    large.push(7);
    assert!(Framer::new().feed(&large).is_err());
}

#[test]
fn rich_protocol_fields_and_extra_buttons_remain_available() {
    for (raw, code, base, kind, text) in [
        ("\x1b[1080::98;5u", 1080, Some(98), KeyEventKind::Press, ""),
        ("\x1b[1080;5:3u", 1080, None, KeyEventKind::Release, ""),
        (
            "\x1b[122:90:122;2;90u",
            122,
            Some(122),
            KeyEventKind::Press,
            "Z",
        ),
        ("\x1b[98;3;8747u", 98, None, KeyEventKind::Press, "∫"),
    ] {
        let Event::Key(key) = &packets(raw.as_bytes())[0].event else {
            panic!("{raw:?}")
        };
        assert_eq!(key.codepoint, Some(code));
        assert_eq!(key.base_code, base);
        assert_eq!(key.event.kind, kind);
        assert_eq!(key.text, text);
        assert_eq!(key.raw, raw.as_bytes());
    }
    let out = packets(b"\x1b[66;48;1080;1;8;1_\x1b[66;48;0;0;8;1_");
    for packet in &out {
        let Event::Key(key) = &packet.event else {
            panic!()
        };
        assert_eq!(key.virtual_key, Some(66));
        assert!(key.event.modifiers.contains(Modifiers::CONTROL));
    }
    let Event::Key(key) = &out[1].event else {
        panic!()
    };
    assert_eq!(key.event.kind, KeyEventKind::Release);
    for (raw, button) in [
        ("\x1b[<128;2;3M", 8),
        ("\x1b[<131;2;3m", 11),
        ("\x1b[96;2;3M", 4),
    ] {
        let Event::Mouse(mouse) = packets(raw.as_bytes())[0].event else {
            panic!()
        };
        assert_eq!(mouse.button, Some(button));
    }
}

#[test]
fn framing_cancellation_limits_and_owned_storage() {
    for raw in [
        b"\x1b]data\x18".as_slice(),
        b"\x1bPdata\x1a",
        b"\x1bXdata\x9c",
        b"\x90data\x9c",
        b"\x9fdata\x9c",
        b"\x9edata\x9c",
        b"\x98data\x9c",
        b"\x1b^data\x1b\\",
        b"\x1b_data\x1b\\",
    ] {
        let out = packets(raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].raw, raw);
    }
    let raw = b"\x1b]cancelled\x1b[A";
    let out = packets(raw);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].raw, b"\x1b]cancelled");
    assert!(matches!(out[1].event, Event::Key(_)));
    let out = packets(b"\x1b[12\x1b[A");
    assert_eq!(out.len(), 2);
    for raw in [
        b"\xff".as_slice(),
        b"\xe2A",
        b"\xf0\x80\x80\x80",
        b"\x1b\xff",
    ] {
        assert_eq!(
            packets(raw)
                .iter()
                .flat_map(|p| p.raw.clone())
                .collect::<Vec<_>>(),
            raw
        );
    }
    let mut f = Framer::new();
    assert!(f.feed(b"\xc2").unwrap().is_empty());
    assert_eq!(f.flush().unwrap().raw, [0xc2]);
    let mut raw = b"a".to_vec();
    let out = f.feed(&raw).unwrap();
    raw[0] = b'b';
    f.feed(b"c").unwrap();
    assert_eq!(out[0].raw, b"a");
    let mut large = b"ok\x1b]".to_vec();
    large.extend(vec![b'a'; 1 << 20]);
    let error = Framer::new().feed(&large).unwrap_err();
    assert_eq!(
        error
            .packets
            .iter()
            .flat_map(|p| p.raw.clone())
            .collect::<Vec<_>>(),
        b"ok"
    );
    let mut many = b"\x1b[".to_vec();
    many.extend(b"1;".repeat(64));
    many.push(b'u');
    assert_eq!(packets(&many)[0].raw, many);
    let mut f = Framer::new();
    f.feed(b"\x1b[200~").unwrap();
    f.feed(b"\x1b[20").unwrap();
    let p = f.flush().unwrap();
    assert!(p.paste);
    assert!(!f.needs_deadline());
}

#[test]
fn decoder_covers_application_keypad_c1_and_win32_records() {
    for raw in [
        b"\x1bO2A".as_slice(),
        b"\x1bOp",
        b"\x9bA",
        b"\x1b[27;5;113~",
        b"\x1b[57364u",
        b"\x1b\x1b",
    ] {
        assert!(matches!(packets(raw)[0].event, Event::Key(_)), "{raw:?}");
    }
    let Event::Key(key) = &packets("界".as_bytes())[0].event else {
        panic!()
    };
    assert_eq!(key.text, "界");
    let Event::Key(key) = &packets(b"\x1bO")[0].event else {
        panic!()
    };
    assert!(
        key.event
            .modifiers
            .contains(Modifiers::ALT | Modifiers::SHIFT)
    );
    for vk in [
        8, 9, 13, 27, 33, 34, 35, 36, 37, 38, 39, 40, 45, 46, 112, 135, 65, 48,
    ] {
        let raw = format!("\x1b[{vk};0;0;1;191;2_");
        let Event::Key(key) = &packets(raw.as_bytes())[0].event else {
            panic!("{raw:?}")
        };
        assert_eq!(key.event.kind, KeyEventKind::Repeat);
        assert!(key.event.modifiers.contains(
            Modifiers::SHIFT
                | Modifiers::ALT
                | Modifiers::CONTROL
                | Modifiers::CAPS_LOCK
                | Modifiers::NUM_LOCK
        ));
    }
    for raw in [
        b"\x1b[1;2;3;4;5;6;7_".as_slice(),
        b"\x1b[bad_",
        b"\x1b[0;0;0;1;0;1_",
        b"\x1b[65;0;1114112;1;0;1_",
        b"\x1b[65;0;65;2;0;1_",
    ] {
        assert!(matches!(packets(raw)[0].event, Event::Unknown), "{raw:?}");
    }
    for (raw, action, button) in [
        (
            b"\x1b[<3;1;1M".as_slice(),
            tui_frame::input::MouseAction::Press,
            None,
        ),
        (b"\x1b[M#!!", tui_frame::input::MouseAction::Release, None),
        (
            b"\x1b[<35;1;1M",
            tui_frame::input::MouseAction::Motion,
            None,
        ),
        (
            b"\x1b[<65;1;1M",
            tui_frame::input::MouseAction::Wheel,
            Some(5),
        ),
        (
            b"\x1b[<1;1;1M",
            tui_frame::input::MouseAction::Press,
            Some(3),
        ),
        (
            b"\x1b[<2;1;1M",
            tui_frame::input::MouseAction::Press,
            Some(2),
        ),
    ] {
        let Event::Mouse(mouse) = packets(raw)[0].event else {
            panic!()
        };
        assert_eq!((mouse.action, mouse.button), (action, button));
    }
}

#[test]
fn complete_ss3_keypad_and_win32_special_keys_are_capture_candidates() {
    for final_byte in b"abcdABCD EFHPQRSMXjklmnopqrstuvwxy"
        .iter()
        .filter(|c| **c != b' ')
    {
        for prefix in [b"\x1bO".as_slice(), b"\x8f", b"\x1bO2"] {
            let raw = [prefix, &[*final_byte]].concat();
            assert!(matches!(packets(&raw)[0].event, Event::Key(_)), "{raw:?}");
        }
    }
    for vk in [
        16, 17, 18, 19, 20, 32, 44, 91, 92, 93, 96, 105, 106, 107, 108, 109, 110, 111, 144, 145,
        160, 161, 162, 163, 164, 165, 173, 174, 175, 176, 177, 178, 179, 186, 187, 188, 189, 190,
        191, 192, 219, 220, 221, 222,
    ] {
        let raw = format!("\x1b[{vk};0;0;1;0;1_");
        assert!(
            matches!(packets(raw.as_bytes())[0].event, Event::Key(_)),
            "{raw:?}"
        );
    }
    let Event::Key(k) = &packets(b"\x1b[13;0;13;1;0;1_")[0].event else {
        panic!()
    };
    assert_eq!(k.event.code, KeyCode::Enter);
}
#[test]
fn kitty_derived_text_respects_shift_lock_keypad_and_explicit_text() {
    for (wire, expected) in [
        ("\x1b[97u", "a"),
        ("\x1b[97;2u", "A"),
        ("\x1b[97;65u", "A"),
        ("\x1b[97:64;2u", "@"),
        ("\x1b[57399u", "0"),
        ("\x1b[57415u", "="),
        ("\x1b[97;5u", ""),
        ("\x1b[97;5;120:121u", "xy"),
    ] {
        let packet = Framer::new().feed(wire.as_bytes()).unwrap().remove(0);
        let Event::Key(key) = packet.event else {
            panic!("expected key: {wire:?}")
        };
        assert_eq!(key.text, expected, "{wire:?}");
    }
}
