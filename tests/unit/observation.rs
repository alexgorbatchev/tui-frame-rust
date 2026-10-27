use super::*;
use std::sync::mpsc;

#[test]
fn selected_streams_have_owned_ordered_offsets_and_drain_before_close() {
    let (send, receive) = mpsc::channel();
    let mut d = Dispatcher::new(
        [EventKind::ChildInput].into_iter().collect(),
        Box::new(move |e| {
            send.send((e.sequence, e.offset, e.bytes)).unwrap();
        }),
        Wake::new().unwrap(),
    );
    let mut ignored = Event::new(EventKind::ChildOutput);
    ignored.bytes = b"ignored".to_vec();
    d.emit(ignored).unwrap();
    for bytes in [b"abc".as_slice(), b"de"] {
        let mut event = Event::new(EventKind::ChildInput);
        event.bytes = bytes.to_vec();
        d.emit(event).unwrap();
    }
    d.close();
    assert_eq!(
        receive.try_iter().collect::<Vec<_>>(),
        vec![(1, 0, b"abc".to_vec()), (2, 3, b"de".to_vec())]
    );
    assert!(matches!(
        d.emit(Event::new(EventKind::ChildInput)),
        Err(Error::SessionClosed)
    ));
}

#[test]
fn slow_observer_is_bounded_and_idle_one_record_can_exceed_byte_budget() {
    let (started, started_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let mut d = Dispatcher::new(
        [EventKind::ChildOutput].into_iter().collect(),
        Box::new(move |_| {
            let _ = started.send(());
            let _ = release_rx.recv();
        }),
        Wake::new().unwrap(),
    );
    let mut event = Event::new(EventKind::ChildOutput);
    event.bytes = vec![0; BYTE_LIMIT + 1];
    d.emit(event).unwrap();
    started_rx.recv().unwrap();
    assert!(matches!(
        d.emit(Event::new(EventKind::ChildOutput)),
        Err(Error::ObservationOverflow)
    ));
    release.send(()).unwrap();
    d.close();
    assert_eq!(d.shared.ledger.lock().unwrap().bytes, 0);
}

#[test]
fn callback_panic_stops_delivery_and_wakes_session() {
    let wake = Wake::new().unwrap();
    let mut d = Dispatcher::new(
        [EventKind::Started].into_iter().collect(),
        Box::new(|_| panic!("observer failed")),
        wake.clone(),
    );
    d.emit(Event::new(EventKind::Started)).unwrap();
    d.close();
    let failure = d.failure().unwrap();
    assert!(failure.contains("observer failed"));
    assert!(failure.contains("PanicTrace"), "{failure}");
    wake.drain().unwrap();
    assert_eq!(d.shared.ledger.lock().unwrap().bytes, 0);
}
