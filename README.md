---
created_on: 2026-10-14 14:40
last_modified: 2026-10-14 14:45
status: current
---

`tui-frame` runs an interactive child inside a terminal with independently updated headers, footers, sidebars, and borders. Rust applications can capture keys, observe owned session snapshots, and inspect complete terminal state without taking over the child's input and output handling.

# What It Does

- **Interactive children**: Runs one command with a controlling PTY and forwards terminal protocols, exact input bytes, and resize signals.
- **Independent regions**: Renders cached regions around the child and coalesces invalidations from callbacks or other threads.
- **Owned observations**: Reports ordered input/output streams, protocol effects, process metadata, captures, and lifecycle events.
- **Terminal inspection**: Exposes cells, hyperlinks, styles, colors, cursor, input modes, and synchronized rendering.
- **Bounded shutdown**: Cancels the owned process session, reaps its leader, drains output with a deadline, and restores the outer terminal.

# How It Works

1. Construct a frame with a command and initial region data, then register the regions and callbacks it needs.
2. Call `run` from an interactive terminal. The frame negotiates its capabilities and runs the child in the remaining viewport.
3. Invalidate regions through a controller to repaint them. Capture callbacks can consume keys before they reach the child.
4. Read the returned child status and any session, draining, or cleanup errors after the terminal has been restored.

# How it Really Works

1. `run` blocks and executes once per frame. The input and output descriptors must identify the same interactive terminal; borrowed descriptors remain owned by the caller. Each terminal has one active console owner.
2. Configuration freezes when execution starts. Border changes and region invalidations remain available through weak controllers, including inside callbacks. A controller does not extend the frame's lifetime.
3. Native terminal snapshots own their cells, styles, and hyperlink bytes. Synchronized output freezes the visible frame while live metadata advances; superseded plain frames are converted only when observed. Hyperlink-bearing held frames retain their URI bytes immediately.
4. Input uses a bounded 1 MiB queue and pauses terminal reads under backpressure. Observations use one callback worker, at most 64 waiting events, and a 16 MiB owned-payload budget. A single event may exceed that budget when the dispatcher is idle. Queue overflow or a callback panic ends the session. A callback that never returns also prevents the observer worker from joining.
5. Cancellation signals only groups freshly observed in the child's session, sends TERM and CONT, then KILL after one second. Output draining after the leader exits is limited to two seconds. A nonzero child exit is a normal `RunResult`; session failures return a `RunError` that retains the child status and cleanup outcomes.
6. Child launch arguments, environment, and directory are captured before execution, preserving Unix non-UTF-8 bytes. Conflicting host-terminal environment hints are removed, and the child receives `TERM=xterm-256color` and `COLORTERM=truecolor`. No network access occurs during a session. Initial native dependency construction downloads a checksum-verified source archive.

# Prerequisites

- Rust 1.96 or newer on Unix. Native runtime checks cover Linux x86-64; macOS runtime behavior is unverified.
- [Zig 0.16.0](https://ziglang.org/download/), `bash`, `curl`, `tar`, `shasum`, and `pkg-config` for native dependency construction.
- [libclang](https://clang.llvm.org/docs/LibClang.html) for native bindings. macOS builds also require the macOS SDK.
- An interactive terminal that reports alternate-screen mode 1049 as inactive before a frame starts.

# Installation

The package is available from its local checkout. From a sibling consumer project:

```sh
cargo add tui-frame --path ../tui-frame
```

# Quick Start

Inspect terminal output without acquiring an interactive terminal:

```rust
use tui_frame::terminal::{Options, Terminal};

fn main() -> tui_frame::Result<()> {
    let mut terminal = Terminal::new(Options::new(40, 10))?;
    terminal.write(b"Hello\x1b[31m world\x1b[0m")?;
    let screen = terminal.snapshot()?;
    assert_eq!(screen.input.cols, 40);
    assert_eq!(screen.input.rows, 10);
    Ok(())
}
```

The runnable `examples/frame.rs` demonstrates a child with a header, border, key capture, and cancellation. It requires an interactive terminal.

# API

| Entry point | Purpose |
| :--- | :--- |
| `frame::Frame::try_new(command, data)` | Construct a frame using a `portable_pty::CommandBuilder`. |
| `Frame::{header,footer,left,right}` | Reserve positive cell dimensions and register region drawing callbacks. |
| `Frame::{capture,observe,observe_events}` | Register key capture and selected owned observations before execution. |
| `Frame::terminal(input, output)` | Select borrowed interactive terminal descriptors. |
| `Frame::controller()` | Obtain a weak, lifetime-independent controller for callbacks and other threads. |
| `Controller::{invalidate,set_border}` | Update region data or border geometry before or during execution. |
| `Frame::run(&Cancellation)` | Execute once and return the child status or an error retaining that status. |
| `terminal::Terminal` | Decode VT output, inspect owned state, resize, and encode native input protocols. |
| `input::Framer` / `routing::Router` | Frame exact wire packets and route them against negotiated terminal modes. |
| `process::{read,list,groups}` | Read native process metadata and session inventories with partial results and field-level errors. |

# Configuration

| Setting | Default | Behavior |
| :--- | :--- | :--- |
| `Frame::inherit_terminal(bool)` | `true` | Inherit outer appearance, preferences, and original termios settings for the child. |
| `Frame::set_border(bool)` | `false` | Reserve a one-cell border around the child. |
| `Frame::observe_events(kinds, callback)` | All event kinds with `observe` | Deliver only selected observation kinds; stream offsets count the selected streams. |
| `Cancellation::cancel()` | Uncancelled | Wake the session and begin process cleanup. |
| `NO_COLOR` | Unset | Any nonempty value disables detected colors while retaining text attributes. |
| `TERM`, `COLORTERM`, `CLICOLOR`, `CLICOLOR_FORCE` | Host environment | Select the host color tier together with terminal capabilities. |

# Compatibility

The API uses native Rust values and owned observations. It does not reproduce Go API signatures. The native terminal revision matches `go-tui-frame`'s pinned Ghostty implementation. ANSI 16/256 color quantization uses a maintained native approximation and can choose a different palette entry from Go's HSLuv mapping. Overline attributes remain visible. The terminal cannot distinguish an inherited explicit cursor color from a theme default during restoration.

Native builds target the build host. Cross-compilation and macOS runtime parity require separate verification. The vendored PTY extension and native provenance are documented in [DEPENDENCIES.md](DEPENDENCIES.md).

# License

[MIT](./LICENSE), copyright 2026 Alex Gorbatchev; vendored dependencies retain their own notices.
