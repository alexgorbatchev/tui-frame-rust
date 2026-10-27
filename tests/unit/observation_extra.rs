use super::*;
use crate::{
    frame::{ChildSnapshot, PtySnapshot},
    input::{self, Framer},
    terminal::{ClipboardContent, ClipboardRead, ClipboardWrite, Options, Terminal},
};
use ratatui::layout::Rect;
use std::{
    sync::mpsc,
    time::{Duration, SystemTime},
};

struct Release(Option<mpsc::Sender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
fn paused(
    handler: impl FnMut(Event) + Send + 'static,
) -> (Dispatcher, mpsc::Receiver<()>, Release) {
    let (entered, entered_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let mut handler = handler;
    let mut first = true;
    let dispatcher = Dispatcher::new(
        EventKind::all().into_iter().collect(),
        Box::new(move |event| {
            if first {
                first = false;
                entered.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            handler(event);
        }),
        Wake::new().unwrap(),
    );
    (dispatcher, entered_rx, Release(Some(release)))
}

fn snapshot() -> Snapshot {
    let now = SystemTime::now();
    let mut terminal = Terminal::new(Options::new(3, 2)).unwrap();
    terminal
        .write(b"\x1b[1m\x1b]8;;https://example.test\x1b\\A\x1b]8;;\x1b\\\x1b]2;owned title\x07")
        .unwrap();
    let mut operating_system =
        process::Snapshot::missing(42, std::io::ErrorKind::PermissionDenied.into());
    operating_system.args =
        process::Field::available(vec!["native argument".into()], "argument view");
    operating_system.environment =
        process::Field::available(vec!["KEY=value".into()], "environment view");
    operating_system.resources.native = process::Field::available(
        [("native resource".into(), 7)].into_iter().collect(),
        "kernel accounting",
    );
    let error = Arc::new(std::io::Error::other("unavailable observation"));
    Snapshot {
        child: ChildSnapshot {
            pid: 42,
            executable: "child".into(),
            args: vec!["original".into()],
            environment: vec!["KEY=original".into()],
            directory: ".".into(),
            started_at: now,
            process_state: None,
            session_processes: vec![operating_system.clone()],
            operating_system,
            session_error: Some(error.clone()),
            pty: PtySnapshot {
                window: None,
                settings: None,
                foreground_group: process::Field::available(42, "tcgetpgrp"),
                window_error: Some(error.clone()),
                settings_error: Some(error),
                observed_at: now,
            },
        },
        terminal: terminal.snapshot().unwrap(),
        viewport: Rect::new(0, 0, 3, 2),
        outer: Rect::new(0, 0, 3, 2),
        observed_at: now,
    }
}

#[test]
fn queue_count_boundary_is_nonblocking_and_every_accepted_record_drains_in_order() {
    let (send, receive) = mpsc::channel();
    let (mut dispatcher, entered, release) = paused(move |event| send.send(event).unwrap());
    dispatcher.emit(Event::new(EventKind::Started)).unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    for i in 0..64 {
        let mut event = Event::new(EventKind::ChildOutput);
        event.bytes = vec![i];
        dispatcher.emit(event).unwrap();
    }
    assert!(matches!(
        dispatcher.emit(Event::new(EventKind::Exited)),
        Err(Error::ObservationOverflow)
    ));
    drop(release);
    dispatcher.close();
    let events = receive.try_iter().collect::<Vec<_>>();
    assert_eq!(events.len(), 65);
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, i as u64 + 1);
        if i > 0 {
            assert_eq!(event.bytes, vec![(i - 1) as u8]);
            assert_eq!(event.offset, i as u64 - 1);
        }
    }
    assert_eq!(dispatcher.shared.ledger.lock().unwrap().bytes, 0);
}

#[test]
fn snapshots_keys_and_clipboard_effects_own_their_payload_and_account_before_callback_mutation() {
    let (send, receive) = mpsc::channel();
    let (mut dispatcher, entered, release) = paused(move |mut event| {
        send.send(event.clone()).unwrap();
        if let Some(snapshot) = &mut event.snapshot {
            snapshot.terminal.title = vec![0; 100_000];
        }
        if let Some(key) = &mut event.input {
            key.raw.clear();
        }
    });
    let mut original = Event::new(EventKind::StateChanged);
    original.snapshot = Some(snapshot());
    let packet = Framer::new().feed(b"a").unwrap().pop().unwrap();
    let input::Event::Key(key) = packet.event else {
        panic!("expected native key");
    };
    original.input = Some(key);
    original.effect = Some(Effect::ClipboardWrite(ClipboardWrite {
        location: 0,
        contents: vec![ClipboardContent {
            mime: b"text/plain".to_vec(),
            data: b"owned clipboard".to_vec(),
        }],
        name: b"clipboard".to_vec(),
        granted: true,
        can_remember: false,
    }));
    dispatcher.emit(original.clone()).unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    dispatcher.emit(original.clone()).unwrap();
    original.snapshot.as_mut().unwrap().child.args[0] = "changed".into();
    original.snapshot.as_mut().unwrap().terminal.cells[0].text = "changed".into();
    original.input.as_mut().unwrap().raw[0] = b'z';
    let Some(Effect::ClipboardWrite(effect)) = &mut original.effect else {
        unreachable!()
    };
    effect.contents[0].data.clear();
    drop(release);
    dispatcher.close();
    let events = receive.try_iter().collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    for event in &events {
        let snapshot = event.snapshot.as_ref().unwrap();
        assert_eq!(snapshot.child.args[0], "original");
        assert_eq!(snapshot.terminal.cells[0].text, "A");
        assert_eq!(event.input.as_ref().unwrap().raw, b"a");
        let Some(Effect::ClipboardWrite(effect)) = &event.effect else {
            unreachable!()
        };
        assert_eq!(effect.contents[0].data, b"owned clipboard");
    }
    assert_eq!(dispatcher.shared.ledger.lock().unwrap().bytes, 0);
}

#[test]
fn byte_budget_counts_owned_effect_payloads_and_observed_errors() {
    let oversized = || vec![b'x'; BYTE_LIMIT];
    let effects = vec![
        Effect::Reply(oversized()),
        Effect::Title(oversized()),
        Effect::Directory(oversized()),
        Effect::Notification {
            title: b"title".to_vec(),
            body: oversized(),
        },
        Effect::Unknown {
            tag: 0,
            content: oversized(),
            truncated: false,
            terminator: None,
        },
        Effect::SemanticPrompt {
            kind: 0,
            prompt_kind: 0,
            exit_code: None,
            command: oversized(),
            error: b"error".to_vec(),
        },
        Effect::ClipboardRead(ClipboardRead {
            location: 0,
            mimes: vec![oversized()],
            list: true,
            name: b"name".to_vec(),
            granted: false,
            can_remember: true,
        }),
        Effect::ClipboardWrite(ClipboardWrite {
            location: 0,
            contents: vec![ClipboardContent {
                mime: b"application/octet-stream".to_vec(),
                data: oversized(),
            }],
            name: b"name".to_vec(),
            granted: false,
            can_remember: true,
        }),
    ];
    let (mut dispatcher, entered, release) = paused(|_| {});
    dispatcher.emit(Event::new(EventKind::Started)).unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    for effect in effects {
        let mut event = Event::new(EventKind::Protocol);
        event.effect = Some(effect);
        assert!(matches!(
            dispatcher.emit(event),
            Err(Error::ObservationOverflow)
        ));
    }
    let mut error = Event::new(EventKind::Routed);
    error.error = Some("x".repeat(BYTE_LIMIT));
    assert!(matches!(
        dispatcher.emit(error),
        Err(Error::ObservationOverflow)
    ));
    let mut small = Event::new(EventKind::Protocol);
    small.effect = Some(Effect::Bell);
    dispatcher.emit(small).unwrap();
    drop(release);
    dispatcher.close();
    assert_eq!(dispatcher.shared.ledger.lock().unwrap().bytes, 0);
}

#[test]
fn panic_discards_queued_records_releases_budget_and_disables_further_admission() {
    let (send, receive) = mpsc::channel();
    let (mut dispatcher, entered, release) = paused(move |_| {
        send.send(()).unwrap();
        std::panic::panic_any(17_u32);
    });
    dispatcher.emit(Event::new(EventKind::Started)).unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    for _ in 0..4 {
        dispatcher.emit(Event::new(EventKind::ChildOutput)).unwrap();
    }
    let wake = dispatcher.shared.wake.clone();
    drop(release);
    let mut fds = [libc::pollfd {
        fd: wake.fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    assert_eq!(crate::transport::poll(&mut fds, 2000).unwrap(), 1);
    assert!(dispatcher.failure().unwrap().contains("non-string payload"));
    for _ in 0..128 {
        dispatcher.emit(Event::new(EventKind::ChildOutput)).unwrap();
    }
    dispatcher.close();
    assert_eq!(receive.try_iter().count(), 1);
    assert_eq!(dispatcher.shared.ledger.lock().unwrap().bytes, 0);
}
