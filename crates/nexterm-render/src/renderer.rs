//! Top-level renderer: orchestrates glyph atlas, instanced cell pipeline, and frame submission.

use anyhow::Result;
use tracing::{info, warn};
use std::sync::Arc;
use winit::window::Window;

use crate::atlas::GlyphCache;
use crate::pipeline::{CellInstance, CellPipeline, Uniforms};
use nexterm_theme::ResolvedTheme;
use nexterm_vte::grid::{Cell, CellAttrs, Color, CursorStyle, Grid};
use std::collections::HashSet;

use egui_wgpu::ScreenDescriptor;

/// Number of columns reserved for the log-mode gutter (fold indicator, line number, timestamp, separator).
pub const LOG_GUTTER_COLS: usize = 20;

// ---------------------------------------------------------------------------

/// Fallback empty cell for out-of-bounds access during scrollback rendering.
const DEFAULT_CELL: Cell = Cell {
    ch: ' ',
    fg: Color::Default,
    bg: Color::Default,
    attrs: nexterm_vte::grid::CellAttrs::empty(),
};

/// The main GPU renderer.
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    pub width: u32,
    pub height: u32,

    // Glyph atlas
    glyph_cache: GlyphCache,
    atlas_texture: wgpu::Texture,
    atlas_view: wgpu::TextureView,

    // Cell pipeline
    cell_pipeline: CellPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,

    // Instance buffer (reused each frame)
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize, // max cells the buffer can hold
    instance_count: u32,      // cells written this frame

    // Cursor blink state
    pub cursor_blink_visible: bool,

    // Active theme
    pub theme: ResolvedTheme,

    // egui integration
    egui_ctx: egui::Context,
    egui_winit: egui_winit::State,
    egui_renderer: Option<egui_wgpu::Renderer>,
    surface_format: wgpu::TextureFormat,
}

impl Renderer {
    pub async fn new(window: Arc<Window>, font_size: f32, font_family: &str) -> Result<Self> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        // ---- GPU setup (same as before) ----
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("no compatible GPU adapter found"))?;

        info!(
            name = %adapter.get_info().name,
            backend = ?adapter.get_info().backend,
            "GPU adapter selected"
        );

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("nexterm-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    ..Default::default()
                },
                None,
            )
            .await?;

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let present_mode = if surface_caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        info!(format = ?surface_format, present_mode = ?present_mode, "surface configured");

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            present_mode,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        // ---- Glyph atlas (CPU + GPU texture) ----
        let glyph_cache = GlyphCache::new(font_size, font_family);

        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas-tex"),
            size: wgpu::Extent3d {
                width: glyph_cache.atlas_width,
                height: glyph_cache.atlas_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Initial upload of pre-cached ASCII glyphs
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &glyph_cache.atlas_data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(glyph_cache.atlas_width),
                rows_per_image: Some(glyph_cache.atlas_height),
            },
            wgpu::Extent3d {
                width: glyph_cache.atlas_width,
                height: glyph_cache.atlas_height,
                depth_or_array_layers: 1,
            },
        );
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());

        // ---- Pipeline ----
        let cell_pipeline = CellPipeline::new(&device, surface_format);

        let uniforms = Uniforms {
            cell_size: [glyph_cache.cell_width, glyph_cache.cell_height],
            viewport_size: [width as f32, height as f32],
            atlas_size: [glyph_cache.atlas_width as f32, glyph_cache.atlas_height as f32],
            _pad: [0.0; 2],
        };
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let bind_group =
            cell_pipeline.create_bind_group(&device, &uniform_buffer, &atlas_view);

        // ---- Instance buffer (pre-allocated for up to 500×250 cells) ----
        let instance_capacity = 500 * 250;
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cell-instances"),
            size: (instance_capacity * std::mem::size_of::<CellInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // egui integration
        let egui_ctx = egui::Context::default();

        // Load CJK font for egui UI: try system fonts first, then bundled fallback
        {
            let mut fonts = egui::FontDefinitions::default();
            let font_paths: &[&str] = &[
                // Windows
                "C:\\Windows\\Fonts\\SourceHanSansCN-VF.otf",
                "C:\\Windows\\Fonts\\SourceHanSansSC-VF.ttf",
                "C:\\Windows\\Fonts\\SourceHanSansCN-Regular.otf",
                "C:\\Windows\\Fonts\\msyh.ttc",
                "C:\\Windows\\Fonts\\simhei.ttf",
                // macOS
                "/System/Library/Fonts/PingFang.ttc",
                "/System/Library/Fonts/STHeiti Light.ttc",
                "/Library/Fonts/Arial Unicode.ttf",
                // Linux
                "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            ];
            let mut loaded = false;
            for path in font_paths {
                if let Ok(data) = std::fs::read(path) {
                    fonts.font_data.insert(
                        "cjk_ui_font".to_owned(),
                        std::sync::Arc::new(egui::FontData::from_owned(data)),
                    );
                    info!("loaded CJK UI font for egui: {path}");
                    loaded = true;
                    break;
                }
            }
            // Bundled fallback: SourceHanSansCN-Bold.otf compiled into the binary
            if !loaded {
                static BUNDLED_CJK: &[u8] = include_bytes!("../fonts/SourceHanSansCN-Bold.otf");
                fonts.font_data.insert(
                    "cjk_ui_font".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(BUNDLED_CJK.to_vec())),
                );
                info!("loaded bundled CJK UI font for egui");
            }
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                list.insert(0, "cjk_ui_font".to_owned());
            }
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                list.push("cjk_ui_font".to_owned());
            }
            egui_ctx.set_fonts(fonts);
        }

        // Disable Tab-based widget navigation — Tab should go to the terminal PTY
        egui_ctx.style_mut(|style| {
            style.interaction.selectable_labels = false;
        });
        egui_ctx.options_mut(|opts| {
            opts.warn_on_id_clash = false;
            // Disable egui's built-in Ctrl+=/-/0 keyboard zoom so it doesn't
            // collide with our terminal-font zoom handler.
            opts.zoom_with_keyboard = false;
        });

        let egui_winit = egui_winit::State::new(
            egui_ctx.clone(),
            egui_ctx.viewport_id(),
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let egui_renderer = Some(egui_wgpu::Renderer::new(&device, surface_format, None, 1, false));

        Ok(Self {
            surface,
            device,
            queue,
            surface_config,
            width,
            height,
            glyph_cache,
            atlas_texture,
            atlas_view,
            cell_pipeline,
            uniform_buffer,
            bind_group,
            instance_buffer,
            instance_capacity,
            instance_count: 0,
            cursor_blink_visible: true,
            theme: ResolvedTheme::default(),
            egui_ctx,
            egui_winit,
            egui_renderer,
            surface_format,
        })
    }

    // ---- Public API ----

    pub fn cell_size(&self) -> (f32, f32) {
        (self.glyph_cache.cell_width, self.glyph_cache.cell_height)
    }

    pub fn resize(&mut self, new_width: u32, new_height: u32) {
        if new_width == 0 || new_height == 0 {
            return;
        }
        self.width = new_width;
        self.height = new_height;
        self.surface_config.width = new_width;
        self.surface_config.height = new_height;
        self.surface.configure(&self.device, &self.surface_config);

        // Update viewport uniform
        let uniforms = Uniforms {
            cell_size: [self.glyph_cache.cell_width, self.glyph_cache.cell_height],
            viewport_size: [new_width as f32, new_height as f32],
            atlas_size: [
                self.glyph_cache.atlas_width as f32,
                self.glyph_cache.atlas_height as f32,
            ],
            _pad: [0.0; 2],
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        tracing::debug!(width = new_width, height = new_height, "surface resized");
    }

    /// Swap in a pre-built GlyphCache (fast — only GPU resource recreation).
    /// Call this from the main thread after the background thread finishes building.
    pub fn swap_glyph_cache(&mut self, cache: GlyphCache) {
        self.glyph_cache = cache;

        // Recreate atlas texture
        self.atlas_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas-tex"),
            size: wgpu::Extent3d {
                width: self.glyph_cache.atlas_width,
                height: self.glyph_cache.atlas_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &self.glyph_cache.atlas_data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.glyph_cache.atlas_width),
                rows_per_image: Some(self.glyph_cache.atlas_height),
            },
            wgpu::Extent3d {
                width: self.glyph_cache.atlas_width,
                height: self.glyph_cache.atlas_height,
                depth_or_array_layers: 1,
            },
        );
        self.atlas_view = self.atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Rebuild bind group with new atlas view
        self.bind_group = self.cell_pipeline.create_bind_group(
            &self.device,
            &self.uniform_buffer,
            &self.atlas_view,
        );

        // Update uniforms with new cell size
        let uniforms = Uniforms {
            cell_size: [self.glyph_cache.cell_width, self.glyph_cache.cell_height],
            viewport_size: [self.width as f32, self.height as f32],
            atlas_size: [
                self.glyph_cache.atlas_width as f32,
                self.glyph_cache.atlas_height as f32,
            ],
            _pad: [0.0; 2],
        };
        self.queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        info!(
            cell_w = self.glyph_cache.cell_width,
            cell_h = self.glyph_cache.cell_height,
            "glyph cache swapped"
        );
    }

    /// Toggle cursor blink state; returns true if state changed.
    pub fn toggle_cursor_blink(&mut self) -> bool {
        self.cursor_blink_visible = !self.cursor_blink_visible;
        true
    }

    /// Build the instance buffer from the terminal grid and upload to the GPU.
    pub fn update_terminal(&mut self, grid: &Grid) {
        let total_cells = grid.rows * grid.cols + 1; // +1 for possible cursor overlay

        // Grow instance buffer if needed
        if total_cells > self.instance_capacity {
            self.instance_capacity = total_cells + 1024;
            self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cell-instances"),
                size: (self.instance_capacity * std::mem::size_of::<CellInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        let cw = self.glyph_cache.cell_width;
        let ch = self.glyph_cache.cell_height;
        let scrolled = grid.scroll_offset > 0;
        let show_cursor = grid.cursor_visible && self.cursor_blink_visible && !scrolled;

        let mut instances = Vec::with_capacity(total_cells);
        let mut overflow_instances: Vec<CellInstance> = Vec::new();
        for row in 0..grid.rows {
            let row_cells = match grid.viewport_row(row) {
                Some(r) => r,
                None => continue,
            };
            for col in 0..grid.cols {
                let cell = row_cells.get(col).map(|c| c).unwrap_or(&DEFAULT_CELL);

                // Skip WIDE_TAIL: the preceding WIDE cell's 2x quad covers it
                if cell.attrs.contains(CellAttrs::WIDE_TAIL) {
                    continue;
                }

                let is_cursor = show_cursor
                    && row == grid.cursor_row
                    && col == grid.cursor_col
                    && grid.cursor_style == CursorStyle::Block;
                let abs_row = grid.viewport_to_absolute(row);
                let is_selected = grid
                    .selection
                    .as_ref()
                    .map_or(false, |sel| sel.contains(abs_row, col));

                let mut fg = color_to_f32(&cell.fg, true, &self.theme);
                let mut bg = color_to_f32(&cell.bg, false, &self.theme);

                if is_selected {
                    bg = self.theme.selection_bg;
                }
                if is_cursor {
                    std::mem::swap(&mut fg, &mut bg);
                }

                let (atlas_rect, glyph_offset, glyph_span) = if cell.ch > ' ' {
                    if let Some(info) = self.glyph_cache.get_or_insert(cell.ch) {
                        // Expand quad for any glyph noticeably wider than the cell
                        // (Nerd Font icons in the 1.0-1.5x range used to be clipped).
                        // Tiny 5% tolerance avoids spurious expansion for ASCII whose
                        // bitmap exactly matches cell width.
                        let total_w = info.offset_x + info.width as f32;
                        let needed = if total_w > cw * 1.05 {
                            (total_w / cw).ceil().max(1.0)
                        } else {
                            1.0
                        };
                        (
                            [
                                info.atlas_x as f32,
                                info.atlas_y as f32,
                                info.width as f32,
                                info.height as f32,
                            ],
                            [info.offset_x, info.offset_y],
                            needed,
                        )
                    } else {
                        ([0.0; 4], [0.0; 2], 1.0)
                    }
                } else {
                    ([0.0; 4], [0.0; 2], 1.0)
                };

                let is_wide = cell.attrs.contains(CellAttrs::WIDE);
                let span = if is_wide { 2.0_f32.max(glyph_span) } else { glyph_span };

                let inst = CellInstance {
                    pos: [col as f32 * cw, row as f32 * ch],
                    atlas_rect,
                    glyph_offset,
                    fg,
                    bg,
                    cell_span: span,
                    _pad: 0.0,
                };
                if span > 1.0 && !is_wide {
                    // Wide-overflow glyph (e.g. Nerd Font icon): defer so it draws on top
                    overflow_instances.push(inst);
                } else {
                    instances.push(inst);
                }
            }
        }
        instances.append(&mut overflow_instances);

        // Non-block cursor overlay (underline or bar)
        if show_cursor && grid.cursor_style != CursorStyle::Block {
            let cx = grid.cursor_col as f32 * cw;
            let cy = grid.cursor_row as f32 * ch;
            let cursor_fg = self.theme.cursor_bg;
            match grid.cursor_style {
                CursorStyle::Underline => {
                    let bar_h = 2.0f32;
                    instances.push(CellInstance {
                        pos: [cx, cy + ch - bar_h],
                        atlas_rect: [0.0, 0.0, cw, bar_h],
                        glyph_offset: [0.0, 0.0],
                        fg: cursor_fg,
                        bg: cursor_fg,
                        cell_span: 1.0,
                        _pad: 0.0,
                    });
                }
                CursorStyle::Bar => {
                    let bar_w = 2.0f32;
                    instances.push(CellInstance {
                        pos: [cx, cy],
                        atlas_rect: [0.0, 0.0, bar_w, ch],
                        glyph_offset: [0.0, 0.0],
                        fg: cursor_fg,
                        bg: cursor_fg,
                        cell_span: 1.0,
                        _pad: 0.0,
                    });
                }
                _ => {}
            }
        }

        // Upload atlas if new glyphs were rasterized
        if self.glyph_cache.atlas_dirty {
            self.upload_atlas();
            self.glyph_cache.atlas_dirty = false;
        }

        self.instance_count = instances.len() as u32;
        self.queue.write_buffer(
            &self.instance_buffer,
            0,
            bytemuck::cast_slice(&instances),
        );
    }

    /// Build instances for multiple panes at once. Each entry: (grid, x_offset, y_offset, focused).
    /// `find_highlights` contains (abs_row, col) of all search-match cells.
    /// `find_current` contains (abs_row, col) of the *current* (focused) match cells.
    /// `log_mode` enables the log-viewer rendering: gutter with line numbers + timestamps,
    /// block-style left bars, fold indicators, and alternating block backgrounds.
    /// `folded_blocks` contains per-pane block IDs that are collapsed.
    pub fn update_multi_pane(
        &mut self,
        panes: &[(&Grid, f32, f32, bool, usize)],
        find_highlights: &HashSet<(usize, usize)>,
        find_current: &HashSet<(usize, usize)>,
        log_mode: bool,
        folded_blocks: &std::collections::HashMap<usize, HashSet<u64>>,
    ) {
        // Log gutter uses LOG_GUTTER_COLS (module constant)

        // Block left-bar colors (cycle through)
        const BLOCK_COLORS: &[[f32; 4]] = &[
            [0.27, 0.52, 0.53, 1.0],  // aqua
            [0.69, 0.38, 0.53, 1.0],  // purple
            [0.84, 0.36, 0.05, 1.0],  // orange
            [0.41, 0.62, 0.42, 1.0],  // green
            [0.60, 0.59, 0.10, 1.0],  // yellow
            [0.80, 0.14, 0.11, 1.0],  // red
        ];

        // Estimate total cells
        let gutter_extra = if log_mode {
            panes.iter().map(|(g, _, _, _, _)| g.rows * LOG_GUTTER_COLS).sum::<usize>()
        } else {
            0
        };
        let total_est: usize = panes
            .iter()
            .map(|(g, _, _, _, _)| g.rows * g.cols)
            .sum::<usize>()
            + gutter_extra
            + 512;

        if total_est > self.instance_capacity {
            self.instance_capacity = total_est + 1024;
            self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cell-instances"),
                size: (self.instance_capacity * std::mem::size_of::<CellInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        let cw = self.glyph_cache.cell_width;
        let ch = self.glyph_cache.cell_height;
        let show_blink = self.cursor_blink_visible;

        // Log mode colors
        let gutter_bg: [f32; 4] = [0.14, 0.14, 0.13, 1.0];
        let gutter_fg: [f32; 4] = [0.57, 0.51, 0.45, 1.0];
        let gutter_ts_fg: [f32; 4] = [0.40, 0.52, 0.42, 1.0];
        let gutter_sep_fg: [f32; 4] = [0.31, 0.29, 0.27, 1.0];
        let fold_fg: [f32; 4] = [0.98, 0.74, 0.18, 1.0]; // bright gold for fold indicator
        let fold_summary_fg: [f32; 4] = [0.57, 0.51, 0.45, 1.0];
        let fold_summary_bg: [f32; 4] = [0.18, 0.17, 0.16, 1.0];
        let block_sep_bg: [f32; 4] = [0.22, 0.21, 0.20, 1.0]; // subtle separator

        let mut instances = Vec::with_capacity(total_est);
        let mut overflow_instances: Vec<CellInstance> = Vec::new();

        let gutter_offset = if log_mode { LOG_GUTTER_COLS as f32 * cw } else { 0.0 };

        let empty_folds: HashSet<u64> = HashSet::new();

        for &(grid, ox, oy, is_focused, pane_id) in panes {
            let scrolled = grid.scroll_offset > 0;
            let show_cursor = grid.cursor_visible && show_blink && is_focused && !scrolled;

            // Disable log mode for panes in alt screen (vim, less, top, etc.)
            let pane_log = log_mode && !grid.is_alt_screen;
            let pane_gutter = if pane_log { gutter_offset } else { 0.0 };
            let pane_folds = folded_blocks.get(&pane_id).unwrap_or(&empty_folds);

            // Decoupled visual/absolute row iteration for fold compression.
            // In log mode, use render_start() which maps virtual scroll offset
            // to the correct absolute row, properly skipping folded blocks.
            let total_abs = grid.total_rows();
            // For fold clipping, always use cursor position when not scrolled
            // (don't depend on show_cursor which includes blink state — that causes flicker).
            let cursor_abs = if !scrolled { grid.scrollback.len() + grid.cursor_row } else { usize::MAX };
            let mut abs_row = if pane_log {
                grid.block_list.render_start(pane_folds, total_abs, cursor_abs, grid.rows, grid.scroll_offset)
            } else {
                grid.viewport_start()
            };
            let mut visual_row: usize = 0;
            let mut cursor_visual_row: Option<usize> = None;

            // In log mode, track current block for coloring and fold state.
            let mut cur_block_idx: usize = 0;
            let mut cur_block_id: u64 = 0;
            // Logical line counter: count non-wrapped rows from row 0 up to
            // the first visible row so wrapped continuations don't consume a
            // line number.
            let mut logical_line: usize = 0;
            if pane_log {
                // Count non-wrapped rows before the viewport start.
                for r in 0..abs_row {
                    if !grid.is_row_wrapped(r) {
                        logical_line += 1;
                    }
                }
                if let Some((bi, blk)) = grid.block_list.block_for_row(abs_row) {
                    cur_block_idx = bi;
                    cur_block_id = blk.id;
                }
            }

            while visual_row < grid.rows && abs_row < total_abs {
                let row_y = oy + visual_row as f32 * ch;

                // In log mode, skip phantom wrap rows (blank rows with stale wrap flag)
                // so deleted wrapped content disappears cleanly. Never skip the cursor row.
                if pane_log && abs_row != cursor_abs && grid.is_phantom_wrap(abs_row) {
                    abs_row += 1;
                    continue;
                }

                // Log mode: detect block boundaries via block list
                let is_block_start = pane_log && grid.is_block_start(abs_row);
                if is_block_start {
                    if let Some((bi, blk)) = grid.block_list.block_for_row(abs_row) {
                        cur_block_idx = bi;
                        cur_block_id = blk.id;
                    }
                }
                let block_color = BLOCK_COLORS[cur_block_idx % BLOCK_COLORS.len()];

                // Check if this row's block is folded (and it's NOT the block-start row)
                let in_folded_block = pane_log
                    && !is_block_start
                    && pane_folds.contains(&cur_block_id);

                // Folded: render one summary line, skip folded rows.
                // Never fold past the cursor row — the input prompt must stay visible.
                if in_folded_block {
                    let mut fold_end = grid.block_list.block_end_row(cur_block_idx, total_abs);
                    // Clip: don't fold past cursor (keeps input visible)
                    if cursor_abs != usize::MAX && fold_end > cursor_abs {
                        fold_end = cursor_abs;
                    }
                    let fold_count = fold_end.saturating_sub(abs_row);
                    if fold_count == 0 {
                        // Nothing to fold (cursor is right here) — render normally
                        // fall through to normal rendering below
                    } else {
                        // Count logical lines (skip soft-wrapped continuations)
                        let logical_lines = (abs_row..fold_end)
                            .filter(|&r| !grid.is_row_wrapped(r))
                            .count();
                        let summary = format!("  --- {} lines folded ---", logical_lines);

                        // Gutter for fold summary
                        for gi in 0..LOG_GUTTER_COLS {
                            instances.push(CellInstance {
                                pos: [ox + gi as f32 * cw, row_y],
                                atlas_rect: [0.0; 4],
                                glyph_offset: [0.0; 2],
                                fg: gutter_fg,
                                bg: fold_summary_bg,
                                cell_span: 1.0,
                                _pad: 0.0,
                            });
                        }
                        // Summary text
                        for (ci, sch) in summary.chars().enumerate() {
                            if ci >= grid.cols { break; }
                            let (atlas_rect, glyph_offset) = if sch > ' ' {
                                if let Some(info) = self.glyph_cache.get_or_insert(sch) {
                                    ([info.atlas_x as f32, info.atlas_y as f32, info.width as f32, info.height as f32], [info.offset_x, info.offset_y])
                                } else { ([0.0; 4], [0.0; 2]) }
                            } else { ([0.0; 4], [0.0; 2]) };
                            instances.push(CellInstance {
                                pos: [ox + pane_gutter + ci as f32 * cw, row_y],
                                atlas_rect,
                                glyph_offset,
                                fg: fold_summary_fg,
                                bg: fold_summary_bg,
                                cell_span: 1.0,
                                _pad: 0.0,
                            });
                        }
                        // Fill rest of row
                        for ci in summary.len()..grid.cols {
                            instances.push(CellInstance {
                                pos: [ox + pane_gutter + ci as f32 * cw, row_y],
                                atlas_rect: [0.0; 4],
                                glyph_offset: [0.0; 2],
                                fg: fold_summary_fg,
                                bg: fold_summary_bg,
                                cell_span: 1.0,
                                _pad: 0.0,
                            });
                        }
                        // Jump past folded rows to next block / cursor
                        abs_row = fold_end;
                        visual_row += 1;
                        continue;
                    }
                }

                // Track cursor visual position for non-block cursor overlay
                if abs_row == cursor_abs {
                    cursor_visual_row = Some(visual_row);
                }

                let row_cells = match grid.absolute_row(abs_row) {
                    Some(r) => r,
                    None => { abs_row += 1; visual_row += 1; continue; }
                };

                // ── Log mode: render gutter with block bar + fold indicator ──
                if pane_log {
                    let is_wrapped = grid.is_row_wrapped(abs_row);

                    // Fold indicator: only for block starts (not on wrapped continuations)
                    let fold_char = if is_block_start && !is_wrapped {
                        if pane_folds.contains(&cur_block_id) { '>' } else { 'v' }
                    } else {
                        ' '
                    };

                    // Wrapped continuations show blank line number and no timestamp.
                    // logical_line only increments for non-wrapped rows.
                    let num_str = if is_wrapped {
                        "   ·".to_string()
                    } else {
                        logical_line += 1;
                        format!("{:>4}", logical_line)
                    };
                    let ts_str = if is_wrapped {
                        "        ".to_string()
                    } else if let Some(ts) = grid.row_timestamp(abs_row) {
                        std::str::from_utf8(ts).unwrap_or("--:--:--").to_string()
                    } else {
                        "        ".to_string()
                    };
                    // Gutter: "F NNNN HH:MM:SS | " (F=fold char, 20 cols total)
                    let gutter_text = format!("{}{} {} | ", fold_char, num_str, ts_str);

                    for (gi, gch) in gutter_text.chars().enumerate().take(LOG_GUTTER_COLS) {
                        let gx = ox + gi as f32 * cw;

                        let fg = if gi == 0 && is_block_start {
                            fold_fg
                        } else if gi >= 6 && gi < 14 {
                            gutter_ts_fg
                        } else if gi == LOG_GUTTER_COLS - 2 {
                            gutter_sep_fg
                        } else {
                            gutter_fg
                        };

                        let (atlas_rect, glyph_offset) = if gch > ' ' {
                            if let Some(info) = self.glyph_cache.get_or_insert(gch) {
                                ([info.atlas_x as f32, info.atlas_y as f32, info.width as f32, info.height as f32], [info.offset_x, info.offset_y])
                            } else { ([0.0; 4], [0.0; 2]) }
                        } else { ([0.0; 4], [0.0; 2]) };

                        instances.push(CellInstance {
                            pos: [gx, row_y],
                            atlas_rect,
                            glyph_offset,
                            fg,
                            bg: gutter_bg,
                            cell_span: 1.0,
                            _pad: 0.0,
                        });
                    }

                    // Block left-bar: 2px wide colored stripe
                    let bar_x = ox + pane_gutter - cw * 1.5;
                    instances.push(CellInstance {
                        pos: [bar_x, row_y],
                        atlas_rect: [0.0, 0.0, 3.0, ch],
                        glyph_offset: [0.0, 0.0],
                        fg: block_color,
                        bg: block_color,
                        cell_span: 1.0,
                        _pad: 0.0,
                    });

                    // Block separator: thin line at top of block-start rows
                    if is_block_start && abs_row > 0 {
                        let total_w = (grid.cols as f32 + LOG_GUTTER_COLS as f32) * cw;
                        instances.push(CellInstance {
                            pos: [ox, row_y],
                            atlas_rect: [0.0, 0.0, total_w, 1.0],
                            glyph_offset: [0.0, 0.0],
                            fg: block_sep_bg,
                            bg: block_sep_bg,
                            cell_span: 1.0,
                            _pad: 0.0,
                        });
                    }
                }

                // ── Terminal content cells ──
                let default_bg = color_to_f32(&Color::Default, false, &self.theme);
                let block_tint: Option<[f32; 4]> = if pane_log && cur_block_idx % 2 == 1 {
                    Some([default_bg[0] + 0.02, default_bg[1] + 0.02, default_bg[2] + 0.02, 1.0])
                } else {
                    None
                };

                for col in 0..grid.cols {
                    let cell = row_cells.get(col).map(|c| c).unwrap_or(&DEFAULT_CELL);

                    if cell.attrs.contains(CellAttrs::WIDE_TAIL) {
                        continue;
                    }

                    let is_cursor = show_cursor
                        && abs_row == cursor_abs
                        && col == grid.cursor_col
                        && grid.cursor_style == CursorStyle::Block;
                    let is_selected = grid
                        .selection
                        .as_ref()
                        .map_or(false, |sel| sel.contains(abs_row, col));

                    let mut fg = color_to_f32(&cell.fg, true, &self.theme);
                    let mut bg = color_to_f32(&cell.bg, false, &self.theme);

                    if let Some(tint) = block_tint {
                        if bg == default_bg {
                            bg = tint;
                        }
                    }

                    if is_selected {
                        bg = self.theme.selection_bg;
                    }

                    let cell_key = (abs_row, col);
                    if find_current.contains(&cell_key) {
                        bg = [0.98, 0.74, 0.18, 1.0];
                        fg = [0.16, 0.16, 0.16, 1.0];
                    } else if find_highlights.contains(&cell_key) {
                        bg = [0.60, 0.47, 0.11, 1.0];
                        fg = [0.92, 0.86, 0.70, 1.0];
                    }

                    if is_cursor {
                        std::mem::swap(&mut fg, &mut bg);
                    }

                    let (atlas_rect, glyph_offset, glyph_span) = if cell.ch > ' ' {
                        if let Some(info) = self.glyph_cache.get_or_insert(cell.ch) {
                            // Expand quad for any glyph noticeably wider than the cell
                            // (Nerd Font icons 1.0-1.5x cell were clipped before).
                            let total_w = info.offset_x + info.width as f32;
                            let needed = if total_w > cw * 1.05 {
                                (total_w / cw).ceil().max(1.0)
                            } else {
                                1.0
                            };
                            (
                                [info.atlas_x as f32, info.atlas_y as f32, info.width as f32, info.height as f32],
                                [info.offset_x, info.offset_y],
                                needed,
                            )
                        } else {
                            ([0.0; 4], [0.0; 2], 1.0)
                        }
                    } else {
                        ([0.0; 4], [0.0; 2], 1.0)
                    };

                    let is_wide = cell.attrs.contains(CellAttrs::WIDE);
                    let span = if is_wide { 2.0_f32.max(glyph_span) } else { glyph_span };

                    let inst = CellInstance {
                        pos: [ox + pane_gutter + col as f32 * cw, row_y],
                        atlas_rect,
                        glyph_offset,
                        fg,
                        bg,
                        cell_span: span,
                        _pad: 0.0,
                    };
                    if span > 1.0 && !is_wide {
                        overflow_instances.push(inst);
                    } else {
                        instances.push(inst);
                    }
                }

                abs_row += 1;
                visual_row += 1;
            }

            // Non-block cursor overlay (uses tracked visual position)
            if show_cursor && grid.cursor_style != CursorStyle::Block {
                if let Some(cv_row) = cursor_visual_row {
                    let cx = ox + pane_gutter + grid.cursor_col as f32 * cw;
                    let cy = oy + cv_row as f32 * ch;
                    let cursor_fg = self.theme.cursor_bg;
                    match grid.cursor_style {
                        CursorStyle::Underline => {
                            let bar_h = 2.0f32;
                            instances.push(CellInstance {
                                pos: [cx, cy + ch - bar_h],
                                atlas_rect: [0.0, 0.0, cw, bar_h],
                                glyph_offset: [0.0, 0.0],
                                fg: cursor_fg,
                                bg: cursor_fg,
                                cell_span: 1.0,
                                _pad: 0.0,
                            });
                        }
                        CursorStyle::Bar => {
                            let bar_w = 2.0f32;
                            instances.push(CellInstance {
                                pos: [cx, cy],
                                atlas_rect: [0.0, 0.0, bar_w, ch],
                                glyph_offset: [0.0, 0.0],
                                fg: cursor_fg,
                                bg: cursor_fg,
                                cell_span: 1.0,
                                _pad: 0.0,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }

        // Append wide-overflow glyphs (e.g. Nerd Font icons) last so they
        // render on top of neighboring cell backgrounds and aren't clipped.
        instances.append(&mut overflow_instances);

        // Upload atlas if new glyphs were rasterized
        if self.glyph_cache.atlas_dirty {
            self.upload_atlas();
            self.glyph_cache.atlas_dirty = false;
        }

        self.instance_count = instances.len() as u32;
        self.queue.write_buffer(
            &self.instance_buffer,
            0,
            bytemuck::cast_slice(&instances),
        );
    }

    /// Render a tab bar row at the top. Each tab is rendered as colored cells.
    pub fn render_tab_bar(
        &mut self,
        tabs: &[(String, bool)], // (title, is_active)
        y_offset: f32,
    ) {
        let cw = self.glyph_cache.cell_width;
        let _ch = self.glyph_cache.cell_height;
        let tab_bar_bg = self.theme.tab_bar_bg;
        let active_bg = self.theme.tab_active_bg;
        let tab_fg = self.theme.fg;

        // Fill entire tab bar row with bg
        let total_cols = (self.width as f32 / cw).ceil() as usize;
        let mut instances = Vec::with_capacity(total_cols + 128);

        // Background fill for full row
        for col in 0..total_cols {
            instances.push(CellInstance {
                pos: [col as f32 * cw, y_offset],
                atlas_rect: [0.0; 4],
                glyph_offset: [0.0; 2],
                fg: tab_bar_bg,
                bg: tab_bar_bg,
                cell_span: 1.0,
                _pad: 0.0,
            });
        }

        // Render tab titles
        let mut x_pos = 1.0f32; // 1 cell padding
        for (title, is_active) in tabs {
            let bg = if *is_active { active_bg } else { tab_bar_bg };
            let padded_title = format!(" {} ", title);
            for ch_char in padded_title.chars() {
                let col_px = x_pos * cw;
                let (atlas_rect, glyph_offset) = if ch_char > ' ' {
                    if let Some(info) = self.glyph_cache.get_or_insert(ch_char) {
                        (
                            [
                                info.atlas_x as f32,
                                info.atlas_y as f32,
                                info.width as f32,
                                info.height as f32,
                            ],
                            [info.offset_x, info.offset_y],
                        )
                    } else {
                        ([0.0; 4], [0.0; 2])
                    }
                } else {
                    ([0.0; 4], [0.0; 2])
                };

                instances.push(CellInstance {
                    pos: [col_px, y_offset],
                    atlas_rect,
                    glyph_offset,
                    fg: tab_fg,
                    bg,
                    cell_span: 1.0,
                    _pad: 0.0,
                });
                x_pos += 1.0;
            }
            x_pos += 1.0; // gap between tabs
        }

        // Upload atlas if needed
        if self.glyph_cache.atlas_dirty {
            self.upload_atlas();
            self.glyph_cache.atlas_dirty = false;
        }

        // Append to existing instance buffer (additive)
        let offset_bytes =
            (self.instance_count as usize * std::mem::size_of::<CellInstance>()) as u64;
        let new_count = self.instance_count + instances.len() as u32;

        // Grow buffer if needed
        let needed = new_count as usize;
        if needed > self.instance_capacity {
            // Need to rebuild — this is a rare case
            self.instance_capacity = needed + 1024;
            self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cell-instances"),
                size: (self.instance_capacity * std::mem::size_of::<CellInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            // Note: previous data lost — caller should call update_multi_pane first
        }

        self.queue.write_buffer(
            &self.instance_buffer,
            offset_bytes,
            bytemuck::cast_slice(&instances),
        );
        self.instance_count = new_count;
    }

    /// Pass a winit event to egui. Returns true if egui consumed the event.
    /// Tab/Escape key events are never forwarded to egui unless a text field is focused,
    /// so they always reach the terminal PTY for shell completion etc.
    pub fn handle_egui_event(&mut self, window: &winit::window::Window, event: &winit::event::WindowEvent) -> bool {
        // If Tab or Escape is pressed and no egui text field is focused,
        // don't forward to egui at all — let the terminal handle it.
        let is_terminal_key = matches!(
            event,
            winit::event::WindowEvent::KeyboardInput {
                event: winit::event::KeyEvent {
                    logical_key: winit::keyboard::Key::Named(
                        winit::keyboard::NamedKey::Tab | winit::keyboard::NamedKey::Escape
                    ),
                    state: winit::event::ElementState::Pressed,
                    ..
                },
                ..
            }
        );
        if is_terminal_key && !self.egui_ctx.wants_keyboard_input() {
            return false;
        }

        let resp = self.egui_winit.on_window_event(window, event);
        resp.consumed
    }

    /// Returns true if egui is actively using the pointer (e.g. dragging a resize handle).
    pub fn egui_wants_pointer(&self) -> bool {
        self.egui_ctx.is_using_pointer()
    }

    /// Access the egui context for building UI.
    pub fn egui_ctx(&self) -> &egui::Context {
        &self.egui_ctx
    }

    /// Begin an egui frame. Must be called before drawing UI. Returns raw input.
    pub fn begin_egui_frame(&mut self, window: &winit::window::Window) {
        let raw_input = self.egui_winit.take_egui_input(window);
        self.egui_ctx.begin_pass(raw_input);
    }

    /// End the egui frame and get paint jobs. Call after drawing UI.
    fn end_egui_frame(&mut self, window: &winit::window::Window) -> (Vec<egui::ClippedPrimitive>, egui::TexturesDelta) {
        let output = self.egui_ctx.end_pass();
        self.egui_winit.handle_platform_output(window, output.platform_output);
        let primitives = self.egui_ctx.tessellate(output.shapes, output.pixels_per_point);
        (primitives, output.textures_delta)
    }

    pub fn render_frame(&mut self, window: &winit::window::Window) -> Result<()> {
        // End egui frame and collect paint data
        let (paint_jobs, textures_delta) = self.end_egui_frame(window);

        let output = match self.surface.get_current_texture() {
            Ok(tex) => tex,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(());
            }
            Err(wgpu::SurfaceError::OutOfMemory) => anyhow::bail!("GPU out of memory"),
            Err(wgpu::SurfaceError::Timeout) => {
                warn!("surface acquire timeout, skipping frame");
                return Ok(());
            }
            Err(e) => anyhow::bail!("surface error: {e}"),
        };

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame-encoder"),
            });

        // Pass 1: Terminal cells
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cell-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: self.theme.bg[0] as f64,
                            g: self.theme.bg[1] as f64,
                            b: self.theme.bg[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if self.instance_count > 0 {
                pass.set_pipeline(&self.cell_pipeline.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                pass.draw(0..6, 0..self.instance_count);
            }
        }

        // Pass 2: egui overlay
        let screen = ScreenDescriptor {
            size_in_pixels: [self.width, self.height],
            pixels_per_point: window.scale_factor() as f32,
        };

        // Submit cell pass
        self.queue.submit(std::iter::once(encoder.finish()));

        // egui pass
        let mut egui_encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("egui-encoder"),
        });

        let egui_rend = self.egui_renderer.as_mut().expect("egui renderer missing");

        for (id, delta) in &textures_delta.set {
            egui_rend.update_texture(&self.device, &self.queue, *id, delta);
        }

        egui_rend.update_buffers(&self.device, &self.queue, &mut egui_encoder, &paint_jobs, &screen);

        // Use forget_lifetime() to get RenderPass<'static> as required by egui-wgpu 0.31
        let mut pass = egui_encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .forget_lifetime();

        egui_rend.render(&mut pass, &paint_jobs, &screen);
        drop(pass);

        for id in &textures_delta.free {
            egui_rend.free_texture(id);
        }

        self.queue.submit(std::iter::once(egui_encoder.finish()));
        output.present();
        Ok(())
    }

    // ---- Internal helpers ----

    fn upload_atlas(&mut self) {
        let y_min = self.glyph_cache.atlas_dirty_y_min;
        let y_max = self.glyph_cache.atlas_dirty_y_max.min(self.glyph_cache.atlas_height);
        if y_min >= y_max {
            return;
        }

        let aw = self.glyph_cache.atlas_width;
        let offset = (y_min * aw) as usize;
        let size = ((y_max - y_min) * aw) as usize;

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: y_min, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &self.glyph_cache.atlas_data[offset..offset + size],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(aw),
                rows_per_image: Some(y_max - y_min),
            },
            wgpu::Extent3d {
                width: aw,
                height: y_max - y_min,
                depth_or_array_layers: 1,
            },
        );

        // Reset dirty region for next frame (no bind group recreation needed —
        // we wrote to the same texture object).
        self.glyph_cache.atlas_dirty_y_min = u32::MAX;
        self.glyph_cache.atlas_dirty_y_max = 0;
    }
}

// ---------------------------------------------------------------------------
// Color conversion
// ---------------------------------------------------------------------------

fn color_to_f32(color: &Color, is_fg: bool, theme: &ResolvedTheme) -> [f32; 4] {
    match color {
        Color::Default => {
            if is_fg {
                theme.fg
            } else {
                theme.bg
            }
        }
        Color::Indexed(idx) => {
            let i = *idx as usize;
            if i < 16 {
                let [r, g, b] = theme.ansi[i];
                [r, g, b, 1.0]
            } else if i < 232 {
                let i = i - 16;
                let r = (i / 36) as f32 / 5.0;
                let g = ((i / 6) % 6) as f32 / 5.0;
                let b = (i % 6) as f32 / 5.0;
                [r, g, b, 1.0]
            } else {
                let v = ((i - 232) as f32 * 10.0 + 8.0) / 255.0;
                [v, v, v, 1.0]
            }
        }
        Color::Rgb(r, g, b) => [*r as f32 / 255.0, *g as f32 / 255.0, *b as f32 / 255.0, 1.0],
    }
}
