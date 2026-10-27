use portable_pty::CommandBuilder;
use tui_frame::{Cancellation, frame::Frame, input::KeyCode, routing::Capture};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = CommandBuilder::new("/bin/sh");
    command.args([
        "-c",
        "printf 'Child ready. Enter a line; q closes the frame.\\n'; while IFS= read -r line; do printf 'Received: %s\\n' \"$line\"; done",
    ]);
    let frame = Frame::try_new(command, String::from("tui-frame: q to close"))?;
    frame.header(1, |context| {
        context
            .view
            .set_string(0, 0, context.data, ratatui::style::Style::default());
    })?;
    frame.set_border(true)?;
    let cancellation = Cancellation::new();
    let stop = cancellation.clone();
    frame.capture(move |key| {
        if key.event.code == KeyCode::Char('q') {
            stop.cancel();
            Capture::Consume
        } else {
            Capture::Pass
        }
    })?;
    match frame.run(&cancellation) {
        Ok(_) => Ok(()),
        Err(error) if matches!(error.error, tui_frame::Error::Cancelled) => Ok(()),
        Err(error) => Err(Box::new(error)),
    }
}
