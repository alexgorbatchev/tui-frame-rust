use super::*;

impl Terminal {
    pub fn encode_mouse(&mut self, event: Mouse, any_button_pressed: bool) -> Result<Vec<u8>> {
        if event.button.is_some_and(|b| b == 0 || b >= 10) {
            return Err(Error::Unroutable("native mouse button"));
        }
        let c = &mut self.core;
        let (mut width, mut height) = (c.options.cell_width_px, c.options.cell_height_px);
        if width == 0 || height == 0 {
            if c.mode(1016)? {
                return Err(Error::Unroutable("SGR pixels require measured cells"));
            }
            width = 1;
            height = 1;
        }
        let size = GhosttyMouseEncoderSize {
            size: size_of::<GhosttyMouseEncoderSize>(),
            screen_width: width
                .checked_mul(u32::from(c.options.cols))
                .ok_or(Error::Unroutable("mouse surface width"))?,
            screen_height: height
                .checked_mul(u32::from(c.options.rows))
                .ok_or(Error::Unroutable("mouse surface height"))?,
            cell_width: width,
            cell_height: height,
            padding_top: 0,
            padding_bottom: 0,
            padding_left: 0,
            padding_right: 0,
        };
        unsafe {
            ghostty_mouse_encoder_setopt_from_terminal(c.mouse, c.native);
            ghostty_mouse_encoder_setopt(
                c.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                (&size as *const GhosttyMouseEncoderSize).cast(),
            );
            ghostty_mouse_encoder_setopt(
                c.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                (&any_button_pressed as *const bool).cast(),
            );
            ghostty_mouse_encoder_setopt(
                c.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_TRACK_LAST_CELL,
                (&false as *const bool).cast(),
            );
            c.configure_mouse(event);
        }
        c.encoded_mouse()
    }
}

impl Core {
    unsafe fn configure_mouse(&self, event: Mouse) {
        unsafe {
            ghostty_mouse_event_set_action(self.mouse_event, event.action);
            match event.button {
                Some(button) => ghostty_mouse_event_set_button(self.mouse_event, button),
                None => ghostty_mouse_event_clear_button(self.mouse_event),
            }
            ghostty_mouse_event_set_mods(self.mouse_event, event.modifiers);
            ghostty_mouse_event_set_position(
                self.mouse_event,
                GhosttyMousePosition {
                    x: event.x,
                    y: event.y,
                },
            );
        }
    }

    fn encoded_mouse(&self) -> Result<Vec<u8>> {
        let mut output = vec![0_u8; 256];
        let mut len = 0;
        unsafe {
            check(
                ghostty_mouse_encoder_encode(
                    self.mouse,
                    self.mouse_event,
                    output.as_mut_ptr().cast(),
                    output.len(),
                    &mut len,
                ),
                "encode mouse",
            )?;
        }
        output.truncate(len);
        Ok(output)
    }

    pub(super) fn mouse_tracking_mode(&mut self) -> Result<u32> {
        let format = GHOSTTY_MOUSE_FORMAT_SGR;
        let size = GhosttyMouseEncoderSize {
            size: size_of::<GhosttyMouseEncoderSize>(),
            screen_width: 1,
            screen_height: 1,
            cell_width: 1,
            cell_height: 1,
            ..Default::default()
        };
        unsafe {
            ghostty_mouse_encoder_setopt_from_terminal(self.mouse, self.native);
            ghostty_mouse_encoder_setopt(
                self.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_FORMAT,
                (&format as *const u32).cast(),
            );
            ghostty_mouse_encoder_setopt(
                self.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                (&size as *const GhosttyMouseEncoderSize).cast(),
            );
            ghostty_mouse_encoder_setopt(
                self.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                (&true as *const bool).cast(),
            );
            ghostty_mouse_encoder_setopt(
                self.mouse,
                GHOSTTY_MOUSE_ENCODER_OPT_TRACK_LAST_CELL,
                (&false as *const bool).cast(),
            );
        }
        for (action, button, mode) in [
            (0, Some(1), GHOSTTY_MOUSE_TRACKING_NONE),
            (1, Some(1), GHOSTTY_MOUSE_TRACKING_X10),
            (2, Some(1), GHOSTTY_MOUSE_TRACKING_NORMAL),
            (2, None, GHOSTTY_MOUSE_TRACKING_BUTTON),
        ] {
            unsafe {
                self.configure_mouse(Mouse {
                    action,
                    button,
                    modifiers: 0,
                    x: 0.0,
                    y: 0.0,
                });
            }
            if self.encoded_mouse()?.is_empty() {
                return Ok(mode);
            }
        }
        Ok(GHOSTTY_MOUSE_TRACKING_ANY)
    }

    pub(super) fn modify_other_keys_2(&self) -> Result<bool> {
        unsafe {
            ghostty_key_encoder_setopt_from_terminal(self.keys, self.native);
            ghostty_key_encoder_setopt(
                self.keys,
                GHOSTTY_KEY_ENCODER_OPT_KITTY_FLAGS,
                (&0_u8 as *const u8).cast(),
            );
            ghostty_key_event_set_action(self.key_probe, GHOSTTY_KEY_ACTION_PRESS);
            ghostty_key_event_set_key(self.key_probe, GHOSTTY_KEY_A);
            ghostty_key_event_set_mods(self.key_probe, GHOSTTY_MODS_CTRL as u16);
            ghostty_key_event_set_utf8(self.key_probe, c"a".as_ptr(), 1);
            ghostty_key_event_set_unshifted_codepoint(self.key_probe, u32::from('a'));
            let mut output = [0_u8; 64];
            let mut len = 0;
            check(
                ghostty_key_encoder_encode(
                    self.keys,
                    self.key_probe,
                    output.as_mut_ptr().cast(),
                    output.len(),
                    &mut len,
                ),
                "probe keyboard format",
            )?;
            match &output[..len] {
                b"\x01" => Ok(false),
                b"\x1b[27;5;97~" => Ok(true),
                _ => Err(Error::Unroutable("unexpected native keyboard format probe")),
            }
        }
    }
}
