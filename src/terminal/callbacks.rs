use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(super) fn install(core: &mut Core) -> Result<()> {
    for (key, callback) in [
        (GHOSTTY_TERMINAL_OPT_WRITE_PTY, reply as *const c_void),
        (GHOSTTY_TERMINAL_OPT_BELL, bell as *const c_void),
        (GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, title as *const c_void),
        (GHOSTTY_TERMINAL_OPT_PWD_CHANGED, directory as *const c_void),
        (GHOSTTY_TERMINAL_OPT_XTVERSION, version as *const c_void),
        (GHOSTTY_TERMINAL_OPT_SIZE, size as *const c_void),
        (GHOSTTY_TERMINAL_OPT_COLOR_SCHEME, scheme as *const c_void),
        (
            GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT,
            progress as *const c_void,
        ),
        (
            GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION,
            notification as *const c_void,
        ),
        (
            GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE,
            unknown as *const c_void,
        ),
        (GHOSTTY_TERMINAL_OPT_RENDER_HOLD, hold as *const c_void),
        (
            GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT,
            semantic as *const c_void,
        ),
        (GHOSTTY_TERMINAL_OPT_RESET, reset as *const c_void),
        (
            GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ,
            clipboard_read as *const c_void,
        ),
        (
            GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
            clipboard_write as *const c_void,
        ),
    ] {
        core.set_pointer(key, callback)?;
    }
    Ok(())
}

// Native userdata points at a stable Box<Core>. Callbacks are synchronous and
// serialized. Catch application panics before they can unwind through C.
unsafe fn dispatch<T>(context: *mut c_void, f: impl FnOnce(&mut Core) -> Result<T>) -> Option<T> {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        f(unsafe { &mut *context.cast::<Core>() })
    }));
    match outcome {
        Ok(Ok(value)) => Some(value),
        Ok(Err(error)) => {
            unsafe { &mut *context.cast::<Core>() }.callback_error = Some(error);
            None
        }
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string payload".into());
            unsafe { &mut *context.cast::<Core>() }.callback_error =
                Some(Error::CallbackPanic(message));
            None
        }
    }
}

unsafe extern "C" fn reply(_: GhosttyTerminal, context: *mut c_void, bytes: *const u8, len: usize) {
    unsafe {
        dispatch(context, |core| {
            core.effects
                .push(Effect::Reply(copied(GhosttyString { ptr: bytes, len })));
            Ok(())
        });
    }
}
unsafe extern "C" fn bell(_: GhosttyTerminal, context: *mut c_void) {
    unsafe {
        dispatch(context, |core| {
            core.effects.push(Effect::Bell);
            Ok(())
        });
    }
}
unsafe extern "C" fn title(_: GhosttyTerminal, context: *mut c_void) {
    unsafe {
        dispatch(context, |core| {
            let value = core.text(GHOSTTY_TERMINAL_DATA_TITLE)?;
            core.effects.push(Effect::Title(value));
            Ok(())
        });
    }
}
unsafe extern "C" fn directory(_: GhosttyTerminal, context: *mut c_void) {
    unsafe {
        dispatch(context, |core| {
            let value = core.text(GHOSTTY_TERMINAL_DATA_PWD)?;
            core.effects.push(Effect::Directory(value));
            Ok(())
        });
    }
}
unsafe extern "C" fn version(_: GhosttyTerminal, context: *mut c_void) -> GhosttyString {
    unsafe {
        dispatch(context, |core| {
            Ok(native_string(core.options.version.as_bytes()))
        })
    }
    .unwrap_or_default()
}
unsafe extern "C" fn size(
    _: GhosttyTerminal,
    context: *mut c_void,
    output: *mut GhosttySizeReportSize,
) -> bool {
    unsafe {
        dispatch(context, |core| {
            *output = GhosttySizeReportSize {
                rows: core.options.rows,
                columns: core.options.cols,
                cell_width: core.options.cell_width_px,
                cell_height: core.options.cell_height_px,
            };
            Ok(true)
        })
    }
    .unwrap_or(false)
}
unsafe extern "C" fn scheme(
    _: GhosttyTerminal,
    context: *mut c_void,
    output: *mut GhosttyColorScheme,
) -> bool {
    unsafe {
        dispatch(context, |core| {
            if let Some(scheme) = core.options.profile.scheme {
                *output = scheme;
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }
    .unwrap_or(false)
}
unsafe extern "C" fn progress(
    _: GhosttyTerminal,
    context: *mut c_void,
    value: *const GhosttyTerminalProgressReport,
) {
    unsafe {
        dispatch(context, |core| {
            let value = &*value;
            core.effects.push(Effect::Progress {
                state: value.state,
                progress: value.progress,
            });
            Ok(())
        });
    }
}
unsafe extern "C" fn notification(
    _: GhosttyTerminal,
    context: *mut c_void,
    value: *const GhosttyTerminalDesktopNotification,
) {
    unsafe {
        dispatch(context, |core| {
            let value = &*value;
            core.effects.push(Effect::Notification {
                title: copied(value.title),
                body: copied(value.body),
            });
            Ok(())
        });
    }
}
unsafe extern "C" fn unknown(
    _: GhosttyTerminal,
    context: *mut c_void,
    value: *const GhosttyTerminalUnknownSequence,
) {
    unsafe {
        dispatch(context, |core| {
            let value = &*value;
            let effect = match value.tag {
                GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_APC => Effect::Unknown {
                    tag: value.tag,
                    content: copied(value.value.apc.content),
                    truncated: value.value.apc.truncated,
                    terminator: None,
                },
                GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_OSC => Effect::Unknown {
                    tag: value.tag,
                    content: copied(value.value.osc.content),
                    truncated: value.value.osc.truncated,
                    terminator: Some(value.value.osc.terminator),
                },
                _ => return Ok(()),
            };
            core.effects.push(effect);
            Ok(())
        });
    }
}
unsafe extern "C" fn hold(_: GhosttyTerminal, context: *mut c_void, held: bool) {
    unsafe {
        dispatch(context, |core| {
            if held {
                core.preserve_frame()?;
            } else {
                core.hold_pending = false;
            }
            core.held = held;
            core.effects.push(Effect::RenderHold(held));
            Ok(())
        });
    }
}
unsafe extern "C" fn semantic(
    _: GhosttyTerminal,
    context: *mut c_void,
    value: *const GhosttyTerminalSemanticPrompt,
) {
    unsafe {
        dispatch(context, |core| {
            let value = &*value;
            core.effects.push(Effect::SemanticPrompt {
                kind: value.kind,
                prompt_kind: value.prompt_kind,
                exit_code: value.has_exit_code.then_some(value.exit_code),
                command: copied(value.command),
                error: copied(value.error),
            });
            Ok(())
        });
    }
}
unsafe extern "C" fn reset(_: GhosttyTerminal, context: *mut c_void) {
    unsafe {
        dispatch(context, |core| {
            core.effects.push(Effect::Reset);
            Ok(())
        });
    }
}

unsafe extern "C" fn clipboard_write(
    _: GhosttyTerminal,
    context: *mut c_void,
    native: *const GhosttyClipboardWrite,
) {
    let reply = unsafe {
        dispatch(context, |core| {
            let value = &*native;
            let contents = if value.contents_len == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(value.contents, value.contents_len)
                    .iter()
                    .map(|c| ClipboardContent {
                        mime: copied(c.mime),
                        data: copied(c.data),
                    })
                    .collect()
            };
            let request = ClipboardWrite {
                location: value.location,
                contents,
                name: copied(value.name),
                granted: value.granted,
                can_remember: value.can_remember,
            };
            core.effects.push(Effect::ClipboardWrite(request.clone()));
            Ok(core.options.clipboard_write.as_mut().map_or(
                ClipboardWriteReply {
                    result: GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED,
                    remember: false,
                },
                |handler| handler(request),
            ))
        })
    };
    let reply = reply.unwrap_or(ClipboardWriteReply {
        result: GHOSTTY_CLIPBOARD_WRITE_RESULT_IO_ERROR,
        remember: false,
    });
    // Reply can synchronously invoke write_pty. Release the mutable Core
    // borrow above before calling it.
    unsafe {
        if let Some(send) = (*native).reply {
            send(
                native,
                &GhosttyClipboardWriteReply {
                    size: size_of::<GhosttyClipboardWriteReply>(),
                    result: reply.result,
                    remember: reply.remember,
                },
            );
        }
    }
}

unsafe extern "C" fn clipboard_read(
    _: GhosttyTerminal,
    context: *mut c_void,
    native: *const GhosttyClipboardRead,
) {
    let reply = unsafe {
        dispatch(context, |core| {
            let value = &*native;
            let mimes = if value.mimes_len == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(value.mimes, value.mimes_len)
                    .iter()
                    .map(|m| copied(*m))
                    .collect()
            };
            let request = ClipboardRead {
                location: value.location,
                mimes,
                list: value.list,
                name: copied(value.name),
                granted: value.granted,
                can_remember: value.can_remember,
            };
            core.effects.push(Effect::ClipboardRead(request.clone()));
            Ok(core.options.clipboard_read.as_mut().map_or(
                ClipboardReadReply {
                    result: GHOSTTY_CLIPBOARD_READ_RESULT_UNSUPPORTED,
                    contents: Vec::new(),
                    available: Vec::new(),
                    remember: false,
                },
                |handler| handler(request),
            ))
        })
    };
    let reply = reply.unwrap_or(ClipboardReadReply {
        result: GHOSTTY_CLIPBOARD_READ_RESULT_IO_ERROR,
        contents: Vec::new(),
        available: Vec::new(),
        remember: false,
    });
    let contents: Vec<_> = reply
        .contents
        .iter()
        .map(|c| GhosttyClipboardContent {
            mime: native_string(&c.mime),
            data: native_string(&c.data),
        })
        .collect();
    let available: Vec<_> = reply.available.iter().map(|m| native_string(m)).collect();
    unsafe {
        if let Some(send) = (*native).reply {
            send(
                native,
                &GhosttyClipboardReadReply {
                    size: size_of::<GhosttyClipboardReadReply>(),
                    result: reply.result,
                    contents: contents.as_ptr(),
                    contents_len: contents.len(),
                    available: available.as_ptr(),
                    available_len: available.len(),
                    remember: reply.remember,
                },
            );
        }
    }
}
