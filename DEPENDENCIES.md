---
created_on: 2026-10-14 14:40
last_modified: 2026-10-14 14:40
status: current
---

# Native dependency provenance

`scripts/native.sh` derives from the MIT-licensed `go-tui-frame` bootstrap and builds Ghostty revision `33da6848d63b3bba2b4f31ab1531d618f2795192`. Its GitHub source archive must match SHA-256 `bbda18d6f6666ff05dec33974c134483b096fe321ff68a22eba9399b70354de6`. Zig 0.16.0 builds the native static library in `.tmp/native/`; bindings are generated from its public C headers. Ghostty's source archive includes its MIT license and dependency notices.

# Vendored portable-pty

`vendor/portable-pty` contains the complete crates.io `portable-pty` 0.9.0 source. The original crate archive SHA-256 is `b4a596a2b3d2752d94f51fac2d4a96737b8705dddd311a32b9af47211f08671e`. Upstream is [wezterm/wezterm](https://github.com/wezterm/wezterm/tree/main/pty), and its MIT notice is retained in [vendor/portable-pty/LICENSE.md](vendor/portable-pty/LICENSE.md).

The local patch has two purposes:

- `CommandBuilder::iter_full_env()` exposes borrowed `OsStr` key/value pairs from the same native map used to construct the child command. The upstream string iterator filters non-UTF-8 values and cannot provide accurate launch metadata. The extension changes neither environment construction nor execution.
- Unix `pre_exec` marks descriptors above stderr close-on-exec with `close_fds::set_fds_cloexec(3, &[])`, using the maintained MIT-licensed `close_fds` 0.3.2 implementation. Upstream `close_random_fds()` closes Rust's private execution-error pipe before `exec`, masking native execution failures and causing an I/O-safety abort. Keeping the pipe alive until `exec` preserves spawn errors; a successful `exec` closes unwanted descriptors. The helper uses native Linux `close_range` with `CLOSE_RANGE_CLOEXEC` where available and signal-safe platform enumeration and `fcntl` fallbacks.

- Unix wait-status conversion retains the native signal name and returns the shell-conventional `128 + signal number` exit code for signal termination. The upstream conversion returns `1` for every signal; this loses information needed by wrappers forwarding the child's status. Real subprocess tests cover normal exits, SIGTERM, and SIGINT.

Only `src/cmdbuilder.rs`, `src/unix.rs`, `src/lib.rs`, and the normalized `Cargo.toml` differ from the original crate source. The added Unix dependency is recorded in the root `Cargo.lock`.

# Rust dependency roles

| Dependency | Native role |
| :--- | :--- |
| `portable-pty` | Controlling PTY allocation, session creation, command launch, and wait status. |
| `ratatui`, `crossterm` | Region buffers, terminal attributes, and cursor control. |
| `anstyle`, `anstyle-lossy` | Explicit color encoding and ANSI palette quantization. |
| `termina` | Native key decoding enriched with protocol fields absent from its public event type. |
| `signal-hook` | SIGWINCH notification through a signal-safe iterator worker. |
| `libc`, Linux `rustix` | Exact native terminal, process, directory, and descriptor operations. |
| `terminfo` | Host terminal color capability lookup. |
| `bindgen` | Public native C bindings generated during construction. |
| `thiserror` | Typed session errors retaining native error chains. |

See the official [Rust pre-exec contract](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.pre_exec), [close_fds signal-safety contract](https://docs.rs/close_fds/0.3.2/close_fds/#async-signal-safety), and [Linux close_range documentation](https://man7.org/linux/man-pages/man2/close_range.2.html) for the descriptor patch requirements.
