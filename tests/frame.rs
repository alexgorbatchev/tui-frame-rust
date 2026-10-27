use portable_pty::CommandBuilder;
use tui_frame::{
    Error,
    frame::{Edge, Frame},
};

#[test]
fn regions_layout_invalidation_and_border_reserve_real_cells() {
    let frame = Frame::new(CommandBuilder::new("/bin/cat"), 0_u32);
    frame
        .header(2, |context| {
            context.view.set_string(
                0,
                0,
                format!("{}", context.data),
                ratatui::style::Style::default(),
            );
        })
        .unwrap();
    frame.footer(1, |_| {}).unwrap();
    frame.left(3, |_| {}).unwrap();
    frame.right(4, |_| {}).unwrap();
    frame.set_border(true).unwrap();
    let layout = frame.layout(30, 12).unwrap();
    assert_eq!(layout.child, ratatui::layout::Rect::new(4, 3, 21, 7));
    assert_eq!(
        layout.regions[Edge::Header as usize],
        ratatui::layout::Rect::new(0, 0, 30, 2)
    );
    frame.invalidate(Edge::Header, 3).unwrap();
    frame.invalidate(Edge::Header, 7).unwrap();
    assert!(frame.layout(8, 12).is_err());
    assert!(matches!(
        Frame::new(CommandBuilder::new("/bin/cat"), ()).invalidate(Edge::Header, ()),
        Err(Error::RegionNotConfigured)
    ));
}

#[test]
fn invalid_regions_do_not_change_layout() {
    let frame = Frame::new(CommandBuilder::new("/bin/cat"), ());
    assert!(frame.header(0, |_| {}).is_err());
    assert_eq!(
        frame.layout(1, 1).unwrap().child,
        ratatui::layout::Rect::new(0, 0, 1, 1)
    );
    assert!(frame.layout(0, 1).is_err());
}

#[test]
fn callback_controller_does_not_keep_frame_or_descriptor_borrows_alive() {
    let frame = Frame::new(CommandBuilder::new("/bin/cat"), 0_u32);
    frame.header(1, |_| {}).unwrap();
    let controller = frame.controller();
    let callback = controller.clone();
    frame
        .capture(move |_| {
            callback.invalidate(Edge::Header, 7).unwrap();
            tui_frame::routing::Capture::Consume
        })
        .unwrap();
    controller.set_border(true).unwrap();
    assert_eq!(
        frame.layout(10, 5).unwrap().child,
        ratatui::layout::Rect::new(1, 2, 8, 2)
    );
    drop(frame);
    assert!(matches!(
        controller.invalidate(Edge::Header, 9),
        Err(Error::SessionClosed)
    ));
    assert!(matches!(
        controller.set_border(false),
        Err(Error::SessionClosed)
    ));
}
