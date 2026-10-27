---
created_on: 2026-10-14 14:40
last_modified: 2026-10-14 14:40
status: current
---

# tui-frame

Native terminal sessions and complete VT state with composited frame regions. Parent instructions in `../AGENTS.md` apply.

## Commands

- Build: `just build`
- Tests: `just test`
- Formatting and strict lint: `just lint`
- Coverage: `just coverage`
- Documentation: `just doc`
- Complete checks: `just check`
- Format this package only: `cargo fmt --package tui-frame`

## Native setup

- `build.rs` constructs the checksum-pinned Ghostty library under `.tmp/native` through `scripts/native.sh`; Zig 0.16.0, libclang, bash, curl, tar, shasum, and pkg-config are required. Native macOS builds also require SDK headers.
- Preserve the pinned revision, source checksum, and dependency notices in `DEPENDENCIES.md` when changing native dependencies.
- `vendor/portable-pty` retains upstream source and MIT attribution. Record every local patch in `DEPENDENCIES.md`; keep raw environment iteration consistent with actual launch environment construction.

## Session contracts

- Keep terminal emulation and PTY writes on the session's single owner. Observers receive owned snapshots through their bounded worker; they must not borrow native state.
- Use `Frame::controller()` inside stored callbacks. Capturing a strong frame clone creates an ownership cycle and ties callbacks to borrowed descriptor lifetimes.
- Borrow caller terminal descriptors; restore their termios and negotiated modes without closing them. Only one console may own a terminal at a time.
- Preserve exact Unix bytes in launch metadata and routed input. A terminal key decoder may not discard protocol fields or rewrite passthrough keys.
- Preserve native error causes and typed partial outcomes. A child nonzero exit is a normal process outcome; aggregate independent cleanup failures rather than replacing them with messages.
- Preserve deferred synchronized rendering: native row damage and conversion occur only for observed plain held frames; hold hyperlink URIs before native storage can change.
- Restrict signals to freshly enumerated groups in the owned session. Do not signal detached sessions, infer membership from a stale snapshot, or wait indefinitely for a foreign process holding the slave open.
- PTY pre-exec hooks must remain async-signal-safe and preserve Rust's execution-error pipe until exec. Mark unwanted descriptors close-on-exec instead of closing that pipe.

## Verification boundaries

- Any runtime behavior change requires a corresponding test-file change and at least 90% line coverage; `scripts/` is excluded. Use real local PTYs and subprocesses for lifecycle, descriptor, cancellation, and process tests. Temporarily disable the implementation, verify a relevant failure, restore it, and rerun checks.
- Use the `termctrl` skill for interactive examples and keep captures under `.tmp/`. Leave unrelated terminal-control sessions alone.
- Preserve standing rules here; keep progress, assignments, and test reports in `.tmp/` or the conversation.
- Never publish releases, tags, packages, or production deployments without explicit authorization.
