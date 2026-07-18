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
        self.update_next_frame_time(self.tab_bar.next_attention_frame_due());

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

        if !self.tab_bar_revealed {
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
