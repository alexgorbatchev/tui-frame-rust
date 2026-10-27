use super::*;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct VisualRow {
    cells: Vec<Cell>,
    styles: Vec<Style>,
}

impl Core {
    pub(super) fn input_state(&mut self) -> Result<InputState> {
        let mut result = InputState {
            cols: self.options.cols,
            rows: self.options.rows,
            cell_width_px: self.options.cell_width_px,
            cell_height_px: self.options.cell_height_px,
            held: self.held,
            alternate: self.get::<GhosttyTerminalScreen>(GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN)?
                == GHOSTTY_TERMINAL_SCREEN_ALTERNATE,
            kitty_flags: self.get(GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS)?,
            modify_other_keys_2: self.modify_other_keys_2()?,
            mouse_tracking: self.get(GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING)?,
            mouse_tracking_mode: self.mouse_tracking_mode()?,
            ..Default::default()
        };
        read_modes(
            self,
            INPUT_MODES,
            &mut result.modes,
            &mut result.mode_errors,
        );
        Ok(result)
    }

    pub(super) fn snapshot(&mut self) -> Result<State> {
        if !self.held {
            self.capture_visual()?;
        } else if self.hold_pending || self.visual.is_none() {
            self.convert_visual()?;
            self.hold_pending = false;
        }
        let mut input = self.input_state()?;
        read_modes(self, STATE_MODES, &mut input.modes, &mut input.mode_errors);
        let bar = self.get::<GhosttyTerminalScrollbar>(GHOSTTY_TERMINAL_DATA_SCROLLBAR)?;
        let mut m = GhosttyTerminalMemoryUsage {
            size: size_of::<GhosttyTerminalMemoryUsage>(),
            ..Default::default()
        };
        unsafe {
            check(
                ghostty_terminal_get(
                    self.native,
                    GHOSTTY_TERMINAL_DATA_MEMORY_USAGE,
                    (&mut m as *mut GhosttyTerminalMemoryUsage).cast(),
                ),
                "read memory usage",
            )?;
        }
        let visual = self
            .visual
            .as_ref()
            .expect("successful capture owns a visual frame");
        Ok(State {
            input,
            cells: visual.cells.clone(),
            native_styles: visual.styles.clone(),
            cursor: visual.cursor.clone(),
            colors: visual.colors.clone(),
            title: self.text(GHOSTTY_TERMINAL_DATA_TITLE)?,
            directory: self.text(GHOSTTY_TERMINAL_DATA_PWD)?,
            mouse_shape: self.get(GHOSTTY_TERMINAL_DATA_MOUSE_SHAPE)?,
            scrollbar: (bar.total, bar.offset, bar.len),
            scrollback_rows: self.get(GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS)?,
            total_rows: self.get(GHOSTTY_TERMINAL_DATA_TOTAL_ROWS)?,
            scrollback_max_bytes: self
                .optional_limit(GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_BYTES)?,
            scrollback_max_lines: self
                .optional_limit(GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_LINES)?,
            memory: MemoryUsage {
                compression_supported: m.compression_supported,
                primary_pages: m.primary_pages,
                primary_virtual_bytes: m.primary_virtual_bytes,
                primary_resident_bytes: m.primary_resident_bytes,
                primary_compressed_pages: m.primary_compressed_pages,
                primary_compressed_bytes: m.primary_compressed_bytes,
                primary_image_bytes: m.primary_image_bytes,
                alternate_pages: m.alternate_pages,
                alternate_virtual_bytes: m.alternate_virtual_bytes,
                alternate_resident_bytes: m.alternate_resident_bytes,
                alternate_compressed_pages: m.alternate_compressed_pages,
                alternate_compressed_bytes: m.alternate_compressed_bytes,
                alternate_image_bytes: m.alternate_image_bytes,
            },
            vt_ground: self.get(GHOSTTY_TERMINAL_DATA_VT_GROUND)?,
            vt_processing_error: self.get(GHOSTTY_TERMINAL_DATA_VT_PROCESSING_ERROR)?,
            viewport_active: self.get(GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE)?,
            cursor_pending_wrap: self.get(GHOSTTY_TERMINAL_DATA_CURSOR_PENDING_WRAP)?,
            cursor_at_prompt: self.get(GHOSTTY_TERMINAL_DATA_CURSOR_AT_PROMPT)?,
        })
    }

    fn optional_limit(&self, key: GhosttyTerminalData) -> Result<Option<usize>> {
        let mut value = 0_usize;
        let result =
            unsafe { ghostty_terminal_get(self.native, key, (&mut value as *mut usize).cast()) };
        if result == GHOSTTY_NO_VALUE {
            return Ok(None);
        }
        check(result, "read scrollback limit")?;
        Ok(Some(value))
    }

    pub(super) fn capture_visual(&mut self) -> Result<()> {
        self.update_render()?;
        self.convert_visual()
    }

    fn render_colors(&self) -> Result<Colors> {
        let mut colors = GhosttyRenderStateColors {
            size: size_of::<GhosttyRenderStateColors>(),
            ..Default::default()
        };
        unsafe {
            check(
                ghostty_render_state_get(
                    self.render,
                    GHOSTTY_RENDER_STATE_DATA_COLORS,
                    (&mut colors as *mut GhosttyRenderStateColors).cast(),
                ),
                "read render colors",
            )?;
        }
        Ok(Colors {
            foreground: rgb(colors.foreground),
            background: rgb(colors.background),
            cursor: colors.cursor_has_value.then(|| rgb(colors.cursor)),
            palette: colors.palette.map(rgb),
        })
    }

    fn update_render(&mut self) -> Result<()> {
        self.check_geometry()?;
        unsafe {
            check(
                ghostty_render_state_update(self.render, self.native),
                "update render state",
            )?;
        }
        let colors = self.render_colors()?;
        if self
            .visual
            .as_ref()
            .is_none_or(|visual| visual.colors != colors)
        {
            let dirty = GHOSTTY_RENDER_STATE_DIRTY_FULL;
            unsafe {
                check(
                    ghostty_render_state_set(
                        self.render,
                        GHOSTTY_RENDER_STATE_OPTION_DIRTY,
                        (&dirty as *const GhosttyRenderStateDirty).cast(),
                    ),
                    "invalidate render colors",
                )?;
            }
        }
        Ok(())
    }

    fn begin_rows(&mut self) -> Result<()> {
        unsafe {
            check(
                ghostty_render_state_get(
                    self.render,
                    GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                    (&mut self.rows as *mut GhosttyRenderStateRowIterator).cast(),
                ),
                "begin render rows",
            )
        }
    }

    pub(super) fn preserve_frame(&mut self) -> Result<()> {
        self.update_render()?;
        self.begin_rows()?;
        let mut y = 0;
        while unsafe { ghostty_render_state_row_iterator_next_dirty(self.rows, &mut y) } {
            let mut row = 0_u64;
            let mut linked = false;
            unsafe {
                check(
                    ghostty_render_state_row_get(
                        self.rows,
                        GHOSTTY_RENDER_STATE_ROW_DATA_RAW,
                        (&mut row as *mut GhosttyRow).cast(),
                    ),
                    "read row flags",
                )?;
                check(
                    ghostty_row_get(
                        row,
                        GHOSTTY_ROW_DATA_HYPERLINK,
                        (&mut linked as *mut bool).cast(),
                    ),
                    "read row hyperlink flag",
                )?;
            }
            if linked {
                // Render rows own text and styles, but URI lookup reads the
                // live terminal. Copy linked rows before later bytes replace it.
                self.convert_visual()?;
                self.hold_pending = false;
                return Ok(());
            }
        }
        self.hold_pending = true;
        Ok(())
    }

    fn convert_visual(&mut self) -> Result<()> {
        let colors = self.render_colors()?;
        let mut cursor = GhosttyRenderStateCursor {
            size: size_of::<GhosttyRenderStateCursor>(),
            ..Default::default()
        };
        unsafe {
            check(
                ghostty_render_state_get(
                    self.render,
                    GHOSTTY_RENDER_STATE_DATA_CURSOR,
                    (&mut cursor as *mut GhosttyRenderStateCursor).cast(),
                ),
                "read cursor",
            )?;
        }
        let cols = usize::from(self.options.cols);
        let rows = usize::from(self.options.rows);
        self.visual_rows.resize_with(rows, VisualRow::default);
        self.dirty.resize(rows, true);
        self.begin_rows()?;
        let mut y = 0;
        while unsafe { ghostty_render_state_row_iterator_next_dirty(self.rows, &mut y) } {
            if usize::from(y) >= rows {
                return Err(Error::Geometry {
                    owned: (self.options.cols, self.options.rows),
                    native: (self.options.cols, y),
                });
            }
            let row = self.convert_row(y, &colors)?;
            self.visual_rows[usize::from(y)] = row;
            self.dirty[usize::from(y)] = true;
        }
        let mut visual = Visual {
            cells: Vec::with_capacity(cols * rows),
            styles: vec![Style::default()],
            colors,
            cursor: Cursor {
                x: cursor.viewport_x,
                y: cursor.viewport_y,
                viewport_has_value: cursor.viewport_has_value,
                wide_tail: cursor.wide_tail,
                visible: cursor.visible,
                blinking: cursor.blinking,
                password_input: cursor.password_input,
                style: cursor.visual_style,
            },
        };
        for row in &self.visual_rows {
            let offset = visual.styles.len() - 1;
            visual.styles.extend(row.styles.iter().cloned());
            visual
                .cells
                .extend(row.cells.iter().cloned().map(|mut cell| {
                    if cell.style_index > 0 {
                        cell.style_index += offset;
                    }
                    cell
                }));
        }
        if visual.cells.len() != cols * rows {
            return Err(Error::Geometry {
                owned: (self.options.cols, self.options.rows),
                native: (self.options.cols, (visual.cells.len() / cols) as u16),
            });
        }
        unsafe {
            check(
                ghostty_render_state_clean(self.render),
                "acknowledge render damage",
            )?;
        }
        self.visual = Some(visual);
        Ok(())
    }

    fn convert_row(&mut self, y: u16, colors: &Colors) -> Result<VisualRow> {
        let mut row = VisualRow::default();
        let mut styles = HashMap::<u16, usize>::new();
        unsafe {
            check(
                ghostty_render_state_row_get(
                    self.rows,
                    GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                    (&mut self.cells as *mut GhosttyRenderStateRowCells).cast(),
                ),
                "begin row cells",
            )?;
        }
        for x in 0..self.options.cols {
            unsafe {
                check(
                    ghostty_render_state_row_cells_select(self.cells, x),
                    "select cell",
                )?;
            }
            let raw = self.cell_data::<u64>(GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW)?;
            let wide = raw_data::<u32>(raw, GHOSTTY_CELL_DATA_WIDE)?;
            let mut native_style = GhosttyStyle {
                size: size_of::<GhosttyStyle>(),
                ..Default::default()
            };
            unsafe {
                check(
                    ghostty_render_state_row_cells_get(
                        self.cells,
                        GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                        (&mut native_style as *mut GhosttyStyle).cast(),
                    ),
                    "read native style",
                )?;
            }
            let full_style = style(&native_style);
            let style_id = raw_data::<u16>(raw, GHOSTTY_CELL_DATA_STYLE_ID)?;
            let style_index = if style_id == 0 {
                0
            } else {
                *styles.entry(style_id).or_insert_with(|| {
                    row.styles.push(full_style.clone());
                    row.styles.len()
                })
            };
            let mut style = full_style;
            style.foreground = self.resolved_style_color(style.foreground, colors);
            style.background = self.resolved_style_color(style.background, colors);
            style.underline_color = self.resolved_style_color(style.underline_color, colors);
            if style.foreground == Color::Default
                && self.options.profile.foreground != Some(colors.foreground)
            {
                style.foreground = Color::Rgb(colors.foreground);
            }
            if style.background == Color::Default
                && self.options.profile.background != Some(colors.background)
            {
                style.background = Color::Rgb(colors.background);
            }
            match raw_data::<u32>(raw, GHOSTTY_CELL_DATA_CONTENT_TAG)? {
                GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE => {
                    style.background = self.resolved_style_color(
                        Color::Palette(raw_data(raw, GHOSTTY_CELL_DATA_COLOR_PALETTE)?),
                        colors,
                    );
                }
                GHOSTTY_CELL_CONTENT_BG_COLOR_RGB => {
                    style.background = Color::Rgb(rgb(raw_data(raw, GHOSTTY_CELL_DATA_COLOR_RGB)?));
                }
                _ => (),
            }
            let len =
                self.cell_data::<u32>(GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN)? as usize;
            let mut codepoints = vec![0_u32; len];
            if len > 0 {
                unsafe {
                    check(
                        ghostty_render_state_row_cells_get(
                            self.cells,
                            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                            codepoints.as_mut_ptr().cast(),
                        ),
                        "read grapheme",
                    )?;
                }
            }
            let mut text: String = codepoints.into_iter().filter_map(char::from_u32).collect();
            let width = match wide {
                GHOSTTY_CELL_WIDE_WIDE => 2,
                GHOSTTY_CELL_WIDE_SPACER_TAIL => {
                    text.clear();
                    0
                }
                GHOSTTY_CELL_WIDE_SPACER_HEAD => {
                    text = " ".into();
                    1
                }
                _ => 1,
            };
            if text.is_empty() && width > 0 {
                text = " ".into();
            }
            let hyperlink = if raw_data::<bool>(raw, GHOSTTY_CELL_DATA_HAS_HYPERLINK)? {
                self.hyperlink(x, u32::from(y))?
            } else {
                Vec::new()
            };
            let selected = self.cell_data::<bool>(GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_SELECTED)?;
            row.cells.push(Cell {
                text,
                width,
                style,
                hyperlink,
                raw,
                wide,
                style_index,
                selected,
            });
        }
        Ok(row)
    }

    fn cell_data<T: Default>(&self, key: GhosttyRenderStateRowCellsData) -> Result<T> {
        let mut value = T::default();
        unsafe {
            check(
                ghostty_render_state_row_cells_get(self.cells, key, (&mut value as *mut T).cast()),
                "read cell",
            )?;
        }
        Ok(value)
    }

    fn hyperlink(&self, x: u16, y: u32) -> Result<Vec<u8>> {
        let point = GhosttyPoint {
            tag: GHOSTTY_POINT_TAG_VIEWPORT,
            value: GhosttyPointValue {
                coordinate: GhosttyPointCoordinate { x, y },
            },
        };
        let mut grid = GhosttyGridRef {
            size: size_of::<GhosttyGridRef>(),
            ..Default::default()
        };
        let mut len = 0;
        unsafe {
            check(
                ghostty_terminal_grid_ref(self.native, point, &mut grid),
                "resolve hyperlink cell",
            )?;
            let result = ghostty_grid_ref_hyperlink_uri(&grid, ptr::null_mut(), 0, &mut len);
            if result != GHOSTTY_OUT_OF_SPACE {
                check(result, "measure hyperlink")?;
            }
            let mut bytes = vec![0; len];
            check(
                ghostty_grid_ref_hyperlink_uri(&grid, bytes.as_mut_ptr(), bytes.len(), &mut len),
                "copy hyperlink",
            )?;
            bytes.truncate(len);
            Ok(bytes)
        }
    }

    fn resolved_style_color(&self, color: Color, colors: &Colors) -> Color {
        match color {
            Color::Palette(index)
                if usize::from(index) < self.options.indexed_entries
                    && self.options.profile.palette.get(&index)
                        == Some(&colors.palette[usize::from(index)]) =>
            {
                color
            }
            Color::Palette(index) => Color::Rgb(colors.palette[usize::from(index)]),
            _ => color,
        }
    }
}

fn raw_data<T: Default>(raw: GhosttyCell, key: GhosttyCellData) -> Result<T> {
    let mut value = T::default();
    unsafe {
        check(
            ghostty_cell_get(raw, key, (&mut value as *mut T).cast()),
            "decode packed cell",
        )?;
    }
    Ok(value)
}

fn color(value: GhosttyStyleColor) -> Color {
    match value.tag {
        GHOSTTY_STYLE_COLOR_PALETTE => Color::Palette(unsafe { value.value.palette }),
        GHOSTTY_STYLE_COLOR_RGB => Color::Rgb(rgb(unsafe { value.value.rgb })),
        _ => Color::Default,
    }
}

fn style(value: &GhosttyStyle) -> Style {
    Style {
        foreground: color(value.fg_color),
        background: color(value.bg_color),
        underline_color: color(value.underline_color),
        bold: value.bold,
        italic: value.italic,
        faint: value.faint,
        blink: value.blink,
        inverse: value.inverse,
        invisible: value.invisible,
        strikethrough: value.strikethrough,
        overline: value.overline,
        underline: value.underline,
    }
}
