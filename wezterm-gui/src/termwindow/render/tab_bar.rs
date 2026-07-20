use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::RenderScreenLineParams;
use crate::utilsprites::RenderMetrics;
use config::ConfigHandle;
use mux::renderable::RenderableDimensions;
use wezterm_term::color::ColorAttribute;
use window::color::LinearRgba;

impl crate::TermWindow {
    pub fn paint_tab_bar(&mut self, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        // While any tab shows the indeterminate progress spinner, ask to be
        // repainted when its next frame is due. The paint scheduler wakes us
        // when this is the soonest pending animation and rebuilds the tab bar
        // to advance the frame.
        self.update_next_frame_time(self.tab_bar.next_progress_frame_due());

        if self.config.use_fancy_tab_bar {
            let collapsed_vertical =
                self.resolved_tab_bar_placement().is_vertical() && self.tab_bar_collapsed;
            if collapsed_vertical {
                self.paint_vertical_collapse_button()?;
                return Ok(());
            }

            if self.fancy_tab_bar.is_none() {
                let palette = self.palette().clone();
                let tab_bar = self.build_fancy_tab_bar(&palette)?;
                self.fancy_tab_bar.replace(tab_bar);
            }

            self.ui_items.append(&mut self.paint_fancy_tab_bar()?);

            if self.resolved_tab_bar_placement().is_vertical() && !self.tab_bar_collapsed {
                let strip_width = self.vertical_tab_bar_width();
                let border = self.get_os_border();
                let handle_w = (self.render_metrics.cell_size.width as f32 * 0.4).max(4.0);
                let x = if self.resolved_tab_bar_placement() == config::TabBarPlacement::Right {
                    self.dimensions.pixel_width as f32 - strip_width - border.right.get() as f32
                } else {
                    strip_width + border.left.get() as f32 - handle_w
                };
                self.ui_items.push(crate::termwindow::UIItem {
                    x: x.max(0.) as usize,
                    y: border.top.get() as usize,
                    width: handle_w.max(1.) as usize,
                    height: self.dimensions.pixel_height,
                    item_type: crate::termwindow::UIItemType::TabBarResize,
                });
            }
            self.paint_vertical_collapse_button()?;
            self.paint_tab_hover_card()?;
            return Ok(());
        }

        let border = self.get_os_border();

        let palette = self.palette().clone();
        let tab_bar_height = self.tab_bar_pixel_height()?;
        let tab_bar_y = if self.config.tab_bar_at_bottom {
            ((self.dimensions.pixel_height as f32) - (tab_bar_height + border.bottom.get() as f32))
                .max(0.)
        } else {
            border.top.get() as f32
        };

        // Register the tab bar location
        self.ui_items.append(&mut self.tab_bar.compute_ui_items(
            tab_bar_y as usize,
            self.render_metrics.cell_size.height as usize,
            self.render_metrics.cell_size.width as usize,
        ));

        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;
        let gl_state = self.render_state.as_ref().unwrap();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        let default_bg = palette
            .resolve_bg(ColorAttribute::Default)
            .to_linear()
            .mul_alpha(if window_is_transparent {
                0.
            } else {
                self.config.text_background_opacity
            });

        self.render_screen_line(
            RenderScreenLineParams {
                top_pixel_y: tab_bar_y,
                left_pixel_x: 0.,
                pixel_width: self.dimensions.pixel_width as f32,
                stable_line_idx: None,
                line: self.tab_bar.line(),
                selection: 0..0,
                cursor: &Default::default(),
                palette: &palette,
                dims: &RenderableDimensions {
                    cols: self.dimensions.pixel_width
                        / self.render_metrics.cell_size.width as usize,
                    physical_top: 0,
                    scrollback_rows: 0,
                    scrollback_top: 0,
                    viewport_rows: 1,
                    dpi: self.terminal_size.dpi,
                    pixel_height: self.render_metrics.cell_size.height as usize,
                    pixel_width: self.terminal_size.pixel_width,
                    reverse_video: false,
                },
                config: &self.config,
                cursor_border_color: LinearRgba::default(),
                foreground: palette.foreground.to_linear(),
                pane: None,
                is_active: true,
                selection_fg: LinearRgba::default(),
                selection_bg: LinearRgba::default(),
                cursor_fg: LinearRgba::default(),
                cursor_bg: LinearRgba::default(),
                cursor_is_default_color: true,
                white_space,
                filled_box,
                window_is_transparent,
                default_bg,
                style: None,
                font: None,
                use_pixel_positioning: self.config.experimental_pixel_positioning,
                render_metrics: self.render_metrics,
                shape_key: None,
                password_input: false,
            },
            layers,
        )?;

        Ok(())
    }

    fn paint_vertical_collapse_button(&mut self) -> anyhow::Result<()> {
        use crate::termwindow::box_model::*;
        use crate::termwindow::UIItemType;
        use config::{Dimension, DimensionContext};

        if !self.config.tab_bar_collapsible || !self.resolved_tab_bar_placement().is_vertical() {
            return Ok(());
        }

        let collapsed = self.tab_bar_collapsed;
        let strip_width = self.vertical_tab_bar_width();
        let pixel_width = self.dimensions.pixel_width as f32;
        let pixel_height = self.dimensions.pixel_height as f32;
        let placement = self.resolved_tab_bar_placement();
        let inner_x = if placement == config::TabBarPlacement::Right {
            pixel_width - strip_width
        } else {
            strip_width
        };

        if !collapsed && !self.tab_bar_revealed {
            return Ok(());
        }

        let colors = self
            .config
            .colors
            .as_ref()
            .and_then(|c| c.tab_bar.as_ref())
            .cloned()
            .unwrap_or_default();
        let active = colors.active_tab();
        let btn_bg = active.bg_color;
        let btn_fg = active.fg_color;

        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let chevron = if collapsed { "\u{f054}" } else { "\u{f053}" };
        let button = Element::new(&font, ElementContent::Text(chevron.to_string()))
            .item_type(UIItemType::TabBarCollapse)
            .zindex(20)
            .padding(BoxDimension {
                left: Dimension::Cells(0.4),
                right: Dimension::Cells(0.4),
                top: Dimension::Cells(0.15),
                bottom: Dimension::Cells(0.15),
            })
            .colors(ElementColors {
                border: BorderColor::default(),
                bg: btn_bg.to_linear().into(),
                text: btn_fg.to_linear().into(),
            });

        let mut computed = self.compute_element(
            &LayoutContext {
                height: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: pixel_height,
                    pixel_cell: metrics.cell_size.height as f32,
                },
                width: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: pixel_width,
                    pixel_cell: metrics.cell_size.width as f32,
                },
                bounds: euclid::rect(0., 0., pixel_width, pixel_height),
                metrics: &metrics,
                gl_state: self.render_state.as_ref().unwrap(),
                zindex: 20,
            },
            &button,
        )?;

        let w = computed.bounds.width();
        let h = computed.bounds.height();
        let button_x = if collapsed {
            if placement == config::TabBarPlacement::Right {
                pixel_width - w
            } else {
                0.
            }
        } else {
            inner_x - w / 2.
        };
        computed.translate(euclid::vec2(button_x, (pixel_height - h) / 2.));

        self.render_element(&computed, self.render_state.as_ref().unwrap(), None)?;
        self.ui_items.append(&mut computed.ui_items());
        Ok(())
    }

    pub fn tab_search_content(&self, tab_idx: usize) -> Option<String> {
        if let Some(cached) = self.tab_search_cache.borrow().get(&tab_idx) {
            return cached.clone();
        }
        let computed = self.compute_tab_search_content(tab_idx);
        self.tab_search_cache
            .borrow_mut()
            .insert(tab_idx, computed.clone());
        computed
    }

    pub fn clear_tab_search_cache(&self) {
        self.tab_search_cache.borrow_mut().clear();
    }

    fn compute_tab_search_content(&self, tab_idx: usize) -> Option<String> {
        let mux = mux::Mux::get();
        let tab = mux
            .get_window(self.mux_window_id)
            .and_then(|w| w.get_by_idx(tab_idx).cloned())?;
        let pane = tab.get_active_pane()?;
        let dims = pane.get_dimensions();
        let (_, lines) =
            pane.get_lines(dims.physical_top..dims.physical_top + dims.viewport_rows as isize);
        let joined = lines
            .iter()
            .map(|l| l.as_str().trim_end().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        if joined.trim().is_empty() {
            None
        } else {
            Some(joined)
        }
    }

    fn paint_tab_hover_card(&mut self) -> anyhow::Result<()> {
        use crate::termwindow::box_model::*;
        use crate::termwindow::render::corners::{
            BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
            TOP_RIGHT_ROUNDED_CORNER,
        };
        use config::{Dimension, DimensionContext};
        use mux::pane::CachePolicy;
        use mux::Mux;

        if !self.config.show_tab_hover_preview
            || !self.resolved_tab_bar_placement().is_vertical()
            || self.tab_bar_collapsed
        {
            self.hovered_card_rect = None;
            return Ok(());
        }
        let tab_idx = match self.hovered_tab {
            Some(idx) => idx,
            None => {
                self.hovered_card_rect = None;
                return Ok(());
            }
        };

        if let Some(since) = self.hovered_tab_since {
            let ready_at =
                since + std::time::Duration::from_millis(self.config.tab_hover_preview_delay_ms);
            if std::time::Instant::now() < ready_at {
                self.update_next_frame_time(Some(ready_at));
                return Ok(());
            }
        }

        if let Some((cached_idx, elem)) = self.hovered_card.as_ref() {
            if *cached_idx == tab_idx {
                self.render_element(elem, self.render_state.as_ref().unwrap(), None)?;
                return Ok(());
            }
        }

        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let cell_w = metrics.cell_size.width as f32;
        let cell_h = metrics.cell_size.height as f32;
        let pixel_width = self.dimensions.pixel_width as f32;
        let pixel_height = self.dimensions.pixel_height as f32;

        let card_w = (cell_w * 40.0).min(pixel_width * 0.45).max(cell_w * 16.0);
        let card_h = (card_w * pixel_height / pixel_width).clamp(cell_h * 4.0, pixel_height * 0.85);
        let inner_rows = (card_h / cell_h).floor().max(3.0) as usize;
        let preview_rows = inner_rows.saturating_sub(2).max(1);
        let cols = ((card_w / cell_w) as usize).saturating_sub(3).max(4);

        let truncate = |s: &str, max: usize| -> String {
            if s.chars().count() > max {
                s.chars().take(max.saturating_sub(1)).collect::<String>() + "\u{2026}"
            } else {
                s.to_string()
            }
        };

        let (title, meta, mut preview) = {
            let mux = Mux::get();
            let tab = match mux
                .get_window(self.mux_window_id)
                .and_then(|w| w.get_by_idx(tab_idx).cloned())
            {
                Some(t) => t,
                None => {
                    self.hovered_card_rect = None;
                    return Ok(());
                }
            };
            let pane = match tab.get_active_pane() {
                Some(p) => p,
                None => {
                    self.hovered_card_rect = None;
                    return Ok(());
                }
            };

            let title = truncate(&pane.get_title(), cols);

            let mut meta_parts = vec![];
            if let Some(proc) = pane.get_foreground_process_name(CachePolicy::AllowStale) {
                let short = std::path::Path::new(&proc)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or(proc);
                meta_parts.push(short);
            }
            let pane_count = tab.count_panes().unwrap_or(1);
            meta_parts.push(format!(
                "{} pane{}",
                pane_count,
                if pane_count == 1 { "" } else { "s" }
            ));
            if let Some(cwd) = pane
                .get_current_working_dir(CachePolicy::AllowStale)
                .and_then(|u| u.to_file_path().ok())
            {
                meta_parts.push(cwd.to_string_lossy().to_string());
            }
            let meta = truncate(&meta_parts.join(" \u{b7} "), cols);

            let dims = pane.get_dimensions();
            let (_, lines) =
                pane.get_lines(dims.physical_top..dims.physical_top + dims.viewport_rows as isize);
            let mut texts: Vec<String> = lines
                .iter()
                .map(|l| l.as_str().trim_end().to_string())
                .collect();
            while texts.last().map(|s| s.is_empty()).unwrap_or(false) {
                texts.pop();
            }
            let first = texts.len().saturating_sub(preview_rows);
            let preview: Vec<String> = texts[first..]
                .iter()
                .map(|s| truncate(s, cols))
                .collect();

            (title, meta, preview)
        };

        while preview.len() < preview_rows {
            preview.push(String::new());
        }
        preview.truncate(preview_rows);

        let palette = self.palette().clone();
        let card_bg = palette.background.to_linear();
        let card_fg = palette.foreground.to_linear();
        let border_col = palette.split.to_linear();
        let dim_fg = card_fg.mul_alpha(0.7);

        let line_element = |text: String, color: LinearRgba| -> Element {
            Element::new(&font, ElementContent::Text(text))
                .display(DisplayType::Block)
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: LinearRgba::TRANSPARENT.into(),
                    text: color.into(),
                })
        };

        let mut children = vec![line_element(title, card_fg)];
        if !meta.is_empty() {
            children.push(line_element(meta, dim_fg));
        }
        for line in preview {
            let shown = if line.is_empty() {
                " ".to_string()
            } else {
                line
            };
            children.push(line_element(shown, dim_fg));
        }

        let radius = SizedPoly {
            width: Dimension::Cells(0.3),
            height: Dimension::Cells(0.3),
            poly: TOP_LEFT_ROUNDED_CORNER,
        };
        let card = Element::new(&font, ElementContent::Children(children))
            .display(DisplayType::Block)
            .min_width(Some(Dimension::Pixels(card_w)))
            .max_width(Some(Dimension::Pixels(card_w)))
            .colors(ElementColors {
                border: BorderColor::new(border_col),
                bg: card_bg.into(),
                text: card_fg.into(),
            })
            .padding(BoxDimension {
                left: Dimension::Cells(0.5),
                right: Dimension::Cells(0.5),
                top: Dimension::Cells(0.3),
                bottom: Dimension::Cells(0.3),
            })
            .border(BoxDimension::new(Dimension::Pixels(
                self.config.tab_hover_preview_border_width,
            )))
            .border_corners(Some(Corners {
                top_left: radius.clone(),
                top_right: SizedPoly {
                    poly: TOP_RIGHT_ROUNDED_CORNER,
                    ..radius.clone()
                },
                bottom_left: SizedPoly {
                    poly: BOTTOM_LEFT_ROUNDED_CORNER,
                    ..radius.clone()
                },
                bottom_right: SizedPoly {
                    poly: BOTTOM_RIGHT_ROUNDED_CORNER,
                    ..radius.clone()
                },
            }))
            .zindex(30);

        let mut computed = self.compute_element(
            &LayoutContext {
                height: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: pixel_height,
                    pixel_cell: cell_h,
                },
                width: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: card_w,
                    pixel_cell: cell_w,
                },
                bounds: euclid::rect(0., 0., card_w, pixel_height),
                metrics: &metrics,
                gl_state: self.render_state.as_ref().unwrap(),
                zindex: 30,
            },
            &card,
        )?;

        let w = computed.bounds.width();
        let h = computed.bounds.height();
        let border = self.get_os_border();
        let strip = self.vertical_tab_bar_width();
        let gap = 6.0;
        let x = if self.resolved_tab_bar_placement() == config::TabBarPlacement::Right {
            (pixel_width - strip - border.right.get() as f32 - w - gap).max(0.)
        } else {
            strip + border.left.get() as f32 + gap
        };
        let ty = self.hovered_tab_rect.map(|r| r.1).unwrap_or(border.top.get() as f32);
        let max_y = (pixel_height - h - border.bottom.get() as f32).max(border.top.get() as f32);
        let y = ty.min(max_y).max(border.top.get() as f32);

        computed.translate(euclid::vec2(x, y));
        self.render_element(&computed, self.render_state.as_ref().unwrap(), None)?;
        self.hovered_card_rect = Some((x, y, w, h));
        self.hovered_card = Some((tab_idx, computed));
        Ok(())
    }

    pub fn tab_bar_pixel_height_impl(
        config: &ConfigHandle,
        fontconfig: &wezterm_font::FontConfiguration,
        render_metrics: &RenderMetrics,
    ) -> anyhow::Result<f32> {
        if config.use_fancy_tab_bar {
            let font = fontconfig.title_font()?;
            Ok((font.metrics().cell_height.get() as f32 * 1.75).ceil())
        } else {
            Ok(render_metrics.cell_size.height as f32)
        }
    }

    pub fn tab_bar_pixel_height(&self) -> anyhow::Result<f32> {
        Self::tab_bar_pixel_height_impl(&self.config, &self.fonts, &self.render_metrics)
    }

    pub fn tab_bar_pixel_width_impl(
        config: &ConfigHandle,
        fontconfig: &wezterm_font::FontConfiguration,
        render_metrics: &RenderMetrics,
    ) -> anyhow::Result<f32> {
        let cell_width = if config.use_fancy_tab_bar {
            let font = fontconfig.title_font()?;
            font.metrics().cell_width.get() as f32
        } else {
            render_metrics.cell_size.width as f32
        };
        Ok((config.tab_bar_width as f32 * cell_width).ceil())
    }

    pub fn tab_bar_pixel_width(&self) -> anyhow::Result<f32> {
        if let Some(override_px) = self.tab_bar_width_override {
            return Ok(override_px);
        }
        Self::tab_bar_pixel_width_impl(&self.config, &self.fonts, &self.render_metrics)
    }
}
