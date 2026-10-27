//! Native terminal sessions with composited, independently invalidated regions.
#![doc = include_str!("../README.md")]

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/ghostty.rs"));
}

pub mod console;
mod control;
mod draw;
pub mod frame;
pub mod input;
mod observation;
pub mod process;
pub mod routing;
mod session;
pub mod terminal;
pub use control::Cancellation;
mod transport;
#[cfg(target_os = "macos")]
#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]
pub(crate) mod process_osx {
    include!(concat!(env!("OUT_DIR"), "/process_osx.rs"));
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Combined(#[from] ErrorGroup),
    #[error(transparent)]
    Shared(std::sync::Arc<Error>),
    #[error("{0}")]
    Session(String),
    #[error("frame region is not configured")]
    RegionNotConfigured,
    #[error("frame session is closed")]
    SessionClosed,
    #[error("terminal is too small for the frame")]
    ViewportTooSmall,
    #[error("frame configuration is frozen")]
    ConfigurationFrozen,
    #[error("frame can execute only one session")]
    AlreadyRun,
    #[error("observation queue exceeded its bounded capacity")]
    ObservationOverflow,
    #[error("session was cancelled")]
    Cancelled,
    #[error("native terminal operation {operation} failed ({code})")]
    Native { operation: &'static str, code: i32 },
    #[error("native terminal reported a VT processing error")]
    Processing,
    #[error("terminal dimensions {cols}x{rows} exceed the supported viewport")]
    Dimensions { cols: u16, rows: u16 },
    #[error("child changed its grid to {native:?}, outside the owned viewport {owned:?}")]
    Geometry {
        owned: (u16, u16),
        native: (u16, u16),
    },
    #[error("terminal callback panicked: {0}")]
    CallbackPanic(String),
    #[error("input cannot be represented in the child's negotiated protocol: {0}")]
    Unroutable(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Pty(Box<dyn std::error::Error + Send + Sync>),
}

/// Ordered native failures retained when an operation and its cleanup both fail.
#[derive(Debug)]
pub struct ErrorGroup {
    pub errors: Vec<Error>,
}
impl std::fmt::Display for ErrorGroup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index != 0 {
                formatter.write_str("; ")?;
            }
            std::fmt::Display::fmt(error, formatter)?;
        }
        Ok(())
    }
}
impl std::error::Error for ErrorGroup {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.errors.first().map(|error| error as _)
    }
}
pub(crate) fn append_error(slot: &mut Option<Error>, error: Error) {
    *slot = Some(match slot.take() {
        None => error,
        Some(Error::Combined(mut group)) => {
            group.errors.push(error);
            Error::Combined(group)
        }
        Some(previous) => ErrorGroup {
            errors: vec![previous, error],
        }
        .into(),
    });
}

pub type Result<T> = std::result::Result<T, Error>;
