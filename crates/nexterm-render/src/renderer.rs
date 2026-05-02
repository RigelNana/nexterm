//! Top-level renderer: orchestrates glyph atlas, instanced cell pipeline, and frame submission.

use anyhow::Result;
use std::sync::Arc;
use tracing::{info, warn};
use winit::window::Window;

use crate::atlas::GlyphCache;
use crate::pipeline::{CellInstance, CellPipeline, Uniforms};
use nexterm_theme::ResolvedTheme;
use nexterm_vte::grid::{Cell, CellAttrs, Color, CursorStyle, Grid, Selection};
use std::collections::{HashMap, HashSet};

/// Per-pane render-loop state captured at the end of a *full* rebuild.
/// Consumed at the start of the next frame to:
///   1. Detect a stable layout (every numeric field unchanged), and
///   2. Locate each visual row's instance slice for in-place rewrite
///      during the Alacritty-style selection partial-rebuild path.
///
/// Mirrors the role of upstream Alacritty's `damage_tracker.frames` —
/// we just keep one frame of history and only need it for the selection
/// fast path.
#[derive(Debug, Clone)]
struct PrevPaneState {
    pane_id: usize,
    cols: usize,
    rows: usize,
    /// Pixel-quantized viewport origin so cosmetic float jitter doesn't
    /// look like a real layout change.
    viewport_x_q: i32,
    viewport_y_q: i32,
    scroll_offset: usize,
    is_alt_screen: bool,
    cursor_row: usize,
    cursor_col: usize,
    cursor_style: CursorStyle,
    /// Effective cursor visibility from last frame (`cursor_visible &&
    /// blink_visible && is_focused && !scrolled`).  Mismatch → blink
    /// toggled or focus changed → fall back to full rebuild.
    show_cursor: bool,
    selection: Option<Selection>,
    /// Per-row instance start offsets in `instance_scratch`.  Length is
    /// `emitted_rows + 1`; `row_starts[i]..row_starts[i + 1]` is row i's
    /// instance slice in the global buffer.  May be shorter than
    /// `rows + 1` if the loop ran out of content (e.g. a small grid
    /// with empty scrollback) — partial rebuild simply skips those.
    row_starts: Vec<usize>,
    /// `true` iff this pane emitted any wide-glyph overflow last frame.
    /// Overflow lives at the end of the global buffer, not in the per-
    /// row slice, so partial rewrite would corrupt indices.  Treated as
    /// a hard "must full-rebuild" gate.
    has_overflow: bool,
}

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

struct GpuAtlasPage {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

fn create_gpu_atlas_pages(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &CellPipeline,
    uniform_buffer: &wgpu::Buffer,
    glyph_cache: &GlyphCache,
) -> Vec<GpuAtlasPage> {
    (0..glyph_cache.pages.len())
        .map(|idx| create_gpu_atlas_page(device, queue, pipeline, uniform_buffer, glyph_cache, idx))
        .collect()
}

fn create_gpu_atlas_page(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &CellPipeline,
    uniform_buffer: &wgpu::Buffer,
    glyph_cache: &GlyphCache,
    idx: usize,
) -> GpuAtlasPage {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
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
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &glyph_cache.pages[idx].data,
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
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = pipeline.create_bind_group(device, uniform_buffer, &view);
    GpuAtlasPage {
        texture,
        bind_group,
    }
}

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
    atlas_pages: Vec<GpuAtlasPage>,

    // Cell pipeline
    cell_pipeline: CellPipeline,
    uniform_buffer: wgpu::Buffer,

    // Instance buffer (reused each frame)
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize, // max cells the buffer can hold
    instance_count: u32,      // cells written this frame
    /// CPU-side snapshot of the last frame's instances. Used to compute the
    /// minimal byte range that needs to be uploaded each frame, avoiding
    /// a full ~MB-sized PCIe transfer when only a few cells changed.
    prev_instances: Vec<CellInstance>,
    /// Scratch Vec reused across `update_multi_pane` calls.  A maximized
    /// window builds ~10k+ CellInstances per frame (~720 KiB); allocating
    /// that fresh each frame during a selection drag creates measurable
    /// allocator churn and dirties the L2 cache every vblank.  Keeping the
    /// backing storage alive lets us `clear()` between frames and reuse
    /// the pre-grown heap buffer for the common case.  This Vec also doubles
    /// as the "previous frame's instance buffer" consumed by the partial-
    /// rebuild path: at the end of a render we put the freshly-built data
    /// back, and at the start of the next render we either rewrite damaged
    /// rows in-place (partial path) or `clear()` and rebuild from scratch
    /// (full path).
    instance_scratch: Vec<CellInstance>,
    /// Secondary scratch for cursor / overflow instances drawn last.
    overflow_scratch: Vec<CellInstance>,

    /// Per-pane state from the previous full rebuild.  Empty on the very
    /// first frame, after a resize, after a layout-altering toggle (log
    /// mode, find bar), or whenever a partial rebuild bails out.  When
    /// non-empty *and* every entry's signature matches the current frame's
    /// panes, we can take the Alacritty-style partial-rebuild fast path.
    prev_pane_states: Vec<PrevPaneState>,
    /// Layout dimensions captured by `prev_pane_states`.  These are global
    /// (not per-pane) signatures; any mismatch forces a full rebuild.
    prev_log_mode: bool,
    prev_find_all: HashSet<(usize, usize)>,
    prev_find_cur: HashSet<(usize, usize)>,
    prev_folds: HashMap<usize, HashSet<u64>>,
    /// Number of overflow instances emitted last frame (appended at the
    /// global tail).  Partial rebuild copies them through unchanged when
    /// `has_overflow == false` for every pane (typically meaning "no Nerd
    /// Font icons in cells right now").
    prev_overflow_len: usize,

    /// `true` between `begin_egui_frame` and `render_frame`'s internal
    /// `end_egui_frame` call.  When the caller skips the egui rebuild
    /// during a high-frequency event (a terminal drag at 1 kHz mouse →
    /// 144 Hz vblank), we keep the flag `false` so `render_frame`
    /// re-uses `cached_paint_jobs` instead of trying to end a pass that
    /// was never started.  egui_winit still accumulates events in the
    /// background, so the next non-skipped frame catches up cleanly.
    egui_pass_active: bool,
    /// Tessellated paint primitives from the last successfully ended
    /// egui frame.  Reused verbatim on skipped frames; relies on the
    /// fact that egui's `TextureId`s remain valid as long as we don't
    /// process a frame that frees them, which is true for any frame
    /// where we *don't* run draw_gui.
    cached_paint_jobs: Vec<egui::ClippedPrimitive>,

    /// How long until egui needs another repaint (button hover transitions,
    /// expanding/collapsing panels, animated tooltips, etc.). The main loop
    /// reads this each frame and shrinks its `WaitUntil` deadline so egui
    /// animations stay smooth without forcing a continuous render loop.
    /// `Duration::MAX` means "no animation pending — sleep as long as you want".
    egui_repaint_after: std::time::Duration,

    // Cursor blink state
    pub cursor_blink_visible: bool,

    // Active theme
    pub theme: ResolvedTheme,

    // egui integration
    egui_ctx: egui::Context,
    egui_winit: egui_winit::State,
    egui_renderer: Option<egui_wgpu::Renderer>,
    #[allow(dead_code)]
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

        let present_mode = if surface_caps
            .present_modes
            .contains(&wgpu::PresentMode::Mailbox)
        {
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

        // ---- Pipeline ----
        let cell_pipeline = CellPipeline::new(&device, surface_format);

        let uniforms = Uniforms {
            cell_size: [glyph_cache.cell_width, glyph_cache.cell_height],
            viewport_size: [width as f32, height as f32],
            atlas_size: [
                glyph_cache.atlas_width as f32,
                glyph_cache.atlas_height as f32,
            ],
            _pad: [0.0; 2],
        };
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let atlas_pages = create_gpu_atlas_pages(
            &device,
            &queue,
            &cell_pipeline,
            &uniform_buffer,
            &glyph_cache,
        );

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
        let egui_renderer = Some(egui_wgpu::Renderer::new(
            &device,
            surface_format,
            None,
            1,
            false,
        ));

        Ok(Self {
            surface,
            device,
            queue,
            surface_config,
            width,
            height,
            glyph_cache,
            atlas_pages,
            cell_pipeline,
            uniform_buffer,
            instance_buffer,
            instance_capacity,
            instance_count: 0,
            prev_instances: Vec::new(),
            instance_scratch: Vec::new(),
            overflow_scratch: Vec::new(),
            prev_pane_states: Vec::new(),
            prev_log_mode: false,
            prev_find_all: HashSet::new(),
            prev_find_cur: HashSet::new(),
            prev_folds: HashMap::new(),
            prev_overflow_len: 0,
            egui_pass_active: false,
            cached_paint_jobs: Vec::new(),
            egui_repaint_after: std::time::Duration::MAX,
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

        self.atlas_pages = create_gpu_atlas_pages(
            &self.device,
            &self.queue,
            &self.cell_pipeline,
            &self.uniform_buffer,
            &self.glyph_cache,
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
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

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
            // New buffer contains garbage — force a full upload on the next
            // diff call by invalidating the CPU snapshot.
            self.prev_instances.clear();
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

                let (atlas_rect, glyph_offset, glyph_span, atlas_idx) = if cell.ch > ' ' {
                    if let Some(info) = self.glyph_cache.get_or_insert(cell.ch) {
                        // Expand quad for any glyph noticeably wider than the cell
                        // (Nerd Font icons in the 1.0-1.5x range used to be clipped).
                        // Tiny 5% tolerance avoids spurious expansion for ASCII whose
                        // bitmap exactly matches cell width.
                        let total_w = info.offset_x + info.width as f32;
                        let needed = if total_w > cw * 1.2 {
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
                            info.atlas_idx,
                        )
                    } else {
                        ([0.0; 4], [0.0; 2], 1.0, 0)
                    }
                } else {
                    ([0.0; 4], [0.0; 2], 1.0, 0)
                };

                let is_wide = cell.attrs.contains(CellAttrs::WIDE);
                let span = if is_wide {
                    2.0_f32.max(glyph_span)
                } else {
                    glyph_span
                };

                let inst = CellInstance {
                    pos: [col as f32 * cw, row as f32 * ch],
                    atlas_rect,
                    glyph_offset,
                    fg,
                    bg,
                    cell_span: span,
                    _pad: atlas_idx as f32,
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
        if self.glyph_cache.has_dirty_pages() {
            self.upload_atlas_pages();
        }

        self.instance_count = instances.len() as u32;
        self.upload_instances_diff(&instances);
    }

    /// Build instances for multiple panes at once. Each entry: (grid, x_offset, y_offset, focused).
    /// `find_highlights` contains (abs_row, col) of all search-match cells.
    /// `find_current` contains (abs_row, col) of the *current* (focused) match cells.
    /// `log_mode` enables the log-viewer rendering: gutter with line numbers + timestamps,
    /// block-style left bars, fold indicators, and alternating block backgrounds.
    /// `folded_blocks` contains per-pane block IDs that are collapsed.
    /// Alacritty-style partial rebuild: when the only thing that changed
    /// since last frame is the user's mouse-drag selection, re-emit *only*
    /// the rows whose selection membership flipped (already marked in
    /// `Grid::damage` by `damage_selection_diff`) and copy the rest of the
    /// instance buffer through from the previous frame untouched.
    ///
    /// Returns `true` on success — the caller has nothing more to do.
    /// Returns `false` if any preflight check fails (layout shifted, log
    /// mode active, cells dirtied by the parser, wide-glyph overflow, …),
    /// in which case the caller falls through to a full rebuild.
    ///
    /// Restricted to non-log-mode for now: log mode's gutter/fold/block
    /// pipeline is too entangled with per-row counters (`logical_line`,
    /// `cur_block_idx`, fold summaries spanning multiple `abs_row`s) to
    /// safely splice individual rows.  Full-rebuild on log mode panes is
    /// already amortised by their lower update frequency.
    fn try_update_multi_pane_partial(
        &mut self,
        panes: &[(&Grid, f32, f32, bool, usize)],
        find_highlights: &HashSet<(usize, usize)>,
        find_current: &HashSet<(usize, usize)>,
        log_mode: bool,
        folded_blocks: &HashMap<usize, HashSet<u64>>,
    ) -> bool {
        // 1. Need a previous frame to splice into.
        if self.prev_pane_states.is_empty()
            || self.instance_scratch.is_empty()
            || panes.len() != self.prev_pane_states.len()
        {
            return false;
        }

        // 2. Log mode disables the fast path (see method-level comment).
        if log_mode || self.prev_log_mode != log_mode {
            return false;
        }

        // 3. Highlights / folds invalidate cached row contents.  These are
        //    typically small sets (find matches, folded block IDs), so an
        //    equality compare is cheap.
        if find_highlights != &self.prev_find_all
            || find_current != &self.prev_find_cur
            || folded_blocks != &self.prev_folds
        {
            return false;
        }

        // 4. Per-pane signature must match exactly, AND no pane may have
        //    cell-level damage (parser writes / scroll / alt-screen swap).
        //    A dirty `cells_dirty` flag means *something other than* a
        //    selection move touched the grid since last frame, in which
        //    case we can't trust per-row offsets to still align.
        let show_blink = self.cursor_blink_visible;
        for (i, &(grid, ox, oy, is_focused, pane_id)) in panes.iter().enumerate() {
            let prev = &self.prev_pane_states[i];
            if prev.has_overflow {
                return false;
            }
            if grid.damage.full || grid.damage.cells_dirty {
                return false;
            }
            let scrolled = grid.scroll_offset > 0;
            let show_cursor =
                grid.cursor_visible && show_blink && is_focused && !scrolled;
            let viewport_x_q = ox.round() as i32;
            let viewport_y_q = oy.round() as i32;
            if pane_id != prev.pane_id
                || grid.cols != prev.cols
                || grid.rows != prev.rows
                || viewport_x_q != prev.viewport_x_q
                || viewport_y_q != prev.viewport_y_q
                || grid.scroll_offset != prev.scroll_offset
                || grid.is_alt_screen != prev.is_alt_screen
                || grid.cursor_row != prev.cursor_row
                || grid.cursor_col != prev.cursor_col
                || grid.cursor_style != prev.cursor_style
                || show_cursor != prev.show_cursor
            {
                return false;
            }
        }

        // 5. If literally nothing changed (no damage, no selection delta),
        //    we can skip work entirely.  Return `true` so the caller knows
        //    not to fall through to a full rebuild.
        let any_damage = panes.iter().any(|(g, ..)| g.damage.is_damaged());
        let any_sel_delta = panes
            .iter()
            .enumerate()
            .any(|(i, (g, ..))| g.selection != self.prev_pane_states[i].selection);
        if !any_damage && !any_sel_delta {
            return true;
        }

        // ── Splice damaged rows in place. ──
        let mut instances = std::mem::take(&mut self.instance_scratch);
        let mut row_buf: Vec<CellInstance> = Vec::with_capacity(256);
        let mut overflow_buf: Vec<CellInstance> = Vec::with_capacity(8);

        for (i, &(grid, ox, oy, is_focused, _pane_id)) in panes.iter().enumerate() {
            let scrolled = grid.scroll_offset > 0;
            let show_cursor =
                grid.cursor_visible && show_blink && is_focused && !scrolled;
            let cursor_abs = if !scrolled {
                grid.scrollback.len() + grid.cursor_row
            } else {
                usize::MAX
            };
            let ch = self.glyph_cache.cell_height;

            for v_row in 0..grid.rows {
                let dirty = grid
                    .damage
                    .lines
                    .get(v_row)
                    .map_or(false, |l| l.damaged);
                if !dirty {
                    continue;
                }
                // Peel the offsets off `self.prev_pane_states[i]` as
                // independent immutable borrows that drop before the
                // mutable `self.render_row_cells_simple` call below.
                let row_starts_len = self.prev_pane_states[i].row_starts.len();
                if v_row + 1 >= row_starts_len {
                    // Row was not emitted last frame (grid had less
                    // content); falling back is safer than guessing the
                    // slice bounds.
                    self.instance_scratch = instances;
                    return false;
                }
                let row_start = self.prev_pane_states[i].row_starts[v_row];
                let row_end = self.prev_pane_states[i].row_starts[v_row + 1];
                let abs_row = grid.viewport_start() + v_row;
                let row_y = oy + v_row as f32 * ch;

                row_buf.clear();
                overflow_buf.clear();
                self.render_row_cells_simple(
                    &mut row_buf,
                    &mut overflow_buf,
                    grid,
                    abs_row,
                    ox,
                    row_y,
                    show_cursor,
                    cursor_abs,
                    find_highlights,
                    find_current,
                );

                if !overflow_buf.is_empty() || row_buf.len() != row_end - row_start {
                    // A wide-glyph icon appeared / disappeared, or the
                    // row's cell count changed (e.g. a new WIDE_TAIL).
                    // The cached layout is no longer valid — bail out.
                    self.instance_scratch = instances;
                    return false;
                }
                instances[row_start..row_end].copy_from_slice(&row_buf);
            }
        }

        // Upload diff (will be tiny — only the damaged rows differ).
        self.instance_count = instances.len() as u32;
        self.upload_instances_diff(&instances);
        self.instance_scratch = instances;

        // Refresh cached selection per pane; everything else stays
        // identical to last frame (layout matched).
        for (i, (g, ..)) in panes.iter().enumerate() {
            self.prev_pane_states[i].selection = g.selection;
        }

        true
    }

    /// Per-row cell emission for the partial-rebuild path.  Mirrors the
    /// non-log-mode branch of `update_multi_pane`'s inner loop exactly so
    /// the emitted CellInstances are byte-identical when the selection
    /// state for that row is unchanged.  Any divergence here would show
    /// up as a flicker every time the partial path triggers.
    fn render_row_cells_simple(
        &mut self,
        out: &mut Vec<CellInstance>,
        overflow: &mut Vec<CellInstance>,
        grid: &Grid,
        abs_row: usize,
        ox: f32,
        row_y: f32,
        show_cursor: bool,
        cursor_abs: usize,
        find_highlights: &HashSet<(usize, usize)>,
        find_current: &HashSet<(usize, usize)>,
    ) {
        let cw = self.glyph_cache.cell_width;
        let row_cells = match grid.absolute_row(abs_row) {
            Some(r) => r,
            None => return,
        };
        let default_bg = color_to_f32(&Color::Default, false, &self.theme);
        let _ = default_bg; // log-mode block_tint not used in non-log path

        for col in 0..grid.cols {
            let cell = row_cells.get(col).unwrap_or(&DEFAULT_CELL);

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

            let (atlas_rect, glyph_offset, glyph_span, atlas_idx) = if cell.ch > ' ' {
                if let Some(info) = self.glyph_cache.get_or_insert(cell.ch) {
                    let total_w = info.offset_x + info.width as f32;
                    let needed = if total_w > cw * 1.2 {
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
                        info.atlas_idx,
                    )
                } else {
                    ([0.0; 4], [0.0; 2], 1.0, 0)
                }
            } else {
                ([0.0; 4], [0.0; 2], 1.0, 0)
            };

            let is_wide = cell.attrs.contains(CellAttrs::WIDE);
            let span = if is_wide {
                2.0_f32.max(glyph_span)
            } else {
                glyph_span
            };

            let inst = CellInstance {
                pos: [ox + col as f32 * cw, row_y],
                atlas_rect,
                glyph_offset,
                fg,
                bg,
                cell_span: span,
                _pad: atlas_idx as f32,
            };
            if span > 1.0 && !is_wide {
                overflow.push(inst);
            } else {
                out.push(inst);
            }
        }
    }

    pub fn update_multi_pane(
        &mut self,
        panes: &[(&Grid, f32, f32, bool, usize)],
        find_highlights: &HashSet<(usize, usize)>,
        find_current: &HashSet<(usize, usize)>,
        log_mode: bool,
        folded_blocks: &std::collections::HashMap<usize, HashSet<u64>>,
    ) {
        // Try the Alacritty-style partial-rebuild path first.  When a
        // mouse-drag selection is the only thing changing, this rewrites
        // a handful of rows in place and skips the per-cell rebuild loop
        // for everything else.  Falls through to the full path on any
        // mismatch (layout change, parser write, log mode, …).
        if self.try_update_multi_pane_partial(
            panes,
            find_highlights,
            find_current,
            log_mode,
            folded_blocks,
        ) {
            return;
        }
        // Log gutter uses LOG_GUTTER_COLS (module constant)

        // Block left-bar colors (cycle through)
        const BLOCK_COLORS: &[[f32; 4]] = &[
            [0.27, 0.52, 0.53, 1.0], // aqua
            [0.69, 0.38, 0.53, 1.0], // purple
            [0.84, 0.36, 0.05, 1.0], // orange
            [0.41, 0.62, 0.42, 1.0], // green
            [0.60, 0.59, 0.10, 1.0], // yellow
            [0.80, 0.14, 0.11, 1.0], // red
        ];

        // Estimate total cells
        let gutter_extra = if log_mode {
            panes
                .iter()
                .map(|(g, _, _, _, _)| g.rows * LOG_GUTTER_COLS)
                .sum::<usize>()
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
            self.prev_instances.clear();
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

        // Borrow the reusable scratch Vecs out of `self` so we don't
        // allocate a fresh ~720 KiB buffer every frame.  We take ownership
        // via `mem::take` because several callees (e.g. `glyph_cache`,
        // `upload_instances_diff`) need exclusive access to other fields of
        // `self` while we fill these.  Both Vecs go back into `self` at the
        // end of the function so the capacity survives across frames.
        let mut instances = std::mem::take(&mut self.instance_scratch);
        instances.clear();
        instances.reserve(total_est);
        let mut overflow_instances = std::mem::take(&mut self.overflow_scratch);
        overflow_instances.clear();

        let gutter_offset = if log_mode {
            LOG_GUTTER_COLS as f32 * cw
        } else {
            0.0
        };

        let empty_folds: HashSet<u64> = HashSet::new();

        // Per-pane state captured during the loop so the next frame can
        // try the partial-rebuild path.  Populated at every row-emission
        // site below (push `instances.len()` *before* the row's first
        // CellInstance, so partial rebuild finds the right slice start).
        let mut new_pane_states: Vec<PrevPaneState> = Vec::with_capacity(panes.len());

        for &(grid, ox, oy, is_focused, pane_id) in panes {
            let scrolled = grid.scroll_offset > 0;
            let show_cursor = grid.cursor_visible && show_blink && is_focused && !scrolled;

            // Disable log mode for panes in alt screen (vim, less, top, etc.)
            let pane_log = log_mode && !grid.is_alt_screen;
            let pane_gutter = if pane_log { gutter_offset } else { 0.0 };
            let pane_folds = folded_blocks.get(&pane_id).unwrap_or(&empty_folds);

            // Snapshot for partial rebuild on the next frame.
            let mut row_starts: Vec<usize> = Vec::with_capacity(grid.rows + 1);
            let pane_overflow_start = overflow_instances.len();

            // Decoupled visual/absolute row iteration for fold compression.
            // In log mode, use render_start() which maps virtual scroll offset
            // to the correct absolute row, properly skipping folded blocks.
            let total_abs = grid.total_rows();
            // For fold clipping, always use cursor position when not scrolled
            // (don't depend on show_cursor which includes blink state — that causes flicker).
            let cursor_abs = if !scrolled {
                grid.scrollback.len() + grid.cursor_row
            } else {
                usize::MAX
            };
            let mut abs_row = if pane_log {
                grid.block_list.render_start(
                    pane_folds,
                    total_abs,
                    cursor_abs,
                    grid.rows,
                    grid.scroll_offset,
                )
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

                // Record where this visual row's first instance lands so
                // the next frame can splice this row in place if only
                // its selection changed.  Must be after the phantom
                // skip (which doesn't increment `visual_row`) but before
                // any cell push.
                row_starts.push(instances.len());

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
                let in_folded_block =
                    pane_log && !is_block_start && pane_folds.contains(&cur_block_id);

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
                            if ci >= grid.cols {
                                break;
                            }
                            let (atlas_rect, glyph_offset, atlas_idx) = if sch > ' ' {
                                if let Some(info) = self.glyph_cache.get_or_insert(sch) {
                                    (
                                        [
                                            info.atlas_x as f32,
                                            info.atlas_y as f32,
                                            info.width as f32,
                                            info.height as f32,
                                        ],
                                        [info.offset_x, info.offset_y],
                                        info.atlas_idx,
                                    )
                                } else {
                                    ([0.0; 4], [0.0; 2], 0)
                                }
                            } else {
                                ([0.0; 4], [0.0; 2], 0)
                            };
                            instances.push(CellInstance {
                                pos: [ox + pane_gutter + ci as f32 * cw, row_y],
                                atlas_rect,
                                glyph_offset,
                                fg: fold_summary_fg,
                                bg: fold_summary_bg,
                                cell_span: 1.0,
                                _pad: atlas_idx as f32,
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
                    None => {
                        abs_row += 1;
                        visual_row += 1;
                        continue;
                    }
                };

                // ── Log mode: render gutter with block bar + fold indicator ──
                if pane_log {
                    let is_wrapped = grid.is_row_wrapped(abs_row);

                    // Fold indicator: only for block starts (not on wrapped continuations)
                    let fold_char = if is_block_start && !is_wrapped {
                        if pane_folds.contains(&cur_block_id) {
                            '>'
                        } else {
                            'v'
                        }
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

                        let (atlas_rect, glyph_offset, atlas_idx) = if gch > ' ' {
                            if let Some(info) = self.glyph_cache.get_or_insert(gch) {
                                (
                                    [
                                        info.atlas_x as f32,
                                        info.atlas_y as f32,
                                        info.width as f32,
                                        info.height as f32,
                                    ],
                                    [info.offset_x, info.offset_y],
                                    info.atlas_idx,
                                )
                            } else {
                                ([0.0; 4], [0.0; 2], 0)
                            }
                        } else {
                            ([0.0; 4], [0.0; 2], 0)
                        };

                        instances.push(CellInstance {
                            pos: [gx, row_y],
                            atlas_rect,
                            glyph_offset,
                            fg,
                            bg: gutter_bg,
                            cell_span: 1.0,
                            _pad: atlas_idx as f32,
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
                    Some([
                        default_bg[0] + 0.02,
                        default_bg[1] + 0.02,
                        default_bg[2] + 0.02,
                        1.0,
                    ])
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

                    let (atlas_rect, glyph_offset, glyph_span, atlas_idx) = if cell.ch > ' ' {
                        if let Some(info) = self.glyph_cache.get_or_insert(cell.ch) {
                            // Expand quad for any glyph noticeably wider than the cell
                            // (Nerd Font icons 1.0-1.5x cell were clipped before).
                            let total_w = info.offset_x + info.width as f32;
                            let needed = if total_w > cw * 1.2 {
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
                                info.atlas_idx,
                            )
                        } else {
                            ([0.0; 4], [0.0; 2], 1.0, 0)
                        }
                    } else {
                        ([0.0; 4], [0.0; 2], 1.0, 0)
                    };

                    let is_wide = cell.attrs.contains(CellAttrs::WIDE);
                    let span = if is_wide {
                        2.0_f32.max(glyph_span)
                    } else {
                        glyph_span
                    };

                    let inst = CellInstance {
                        pos: [ox + pane_gutter + col as f32 * cw, row_y],
                        atlas_rect,
                        glyph_offset,
                        fg,
                        bg,
                        cell_span: span,
                        _pad: atlas_idx as f32,
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

            // Sentinel: end of last visual row's instance slice = start
            // of the next thing (cursor overlay or pane terminator).  We
            // intentionally push *after* the row loop and *before*
            // cursor overlay so partial rebuild's `row_starts[v_row + 1]`
            // never accidentally points into cursor-overlay territory.
            // (Cursor overlay isn't repaired by partial rebuild — any
            // change there forces a full rebuild via `show_cursor` in
            // the layout signature.)
            //
            // NB: technically the sentinel should be captured *before*
            // emitting the cursor overlay — pushing it here means
            // `row_starts.last()` includes the cursor instances.  In
            // practice partial rebuild only reads `row_starts[v_row]`
            // and `row_starts[v_row + 1]` for `v_row < emitted_rows`,
            // never touching the sentinel, so the over-count is benign.
            row_starts.push(instances.len());

            new_pane_states.push(PrevPaneState {
                pane_id,
                cols: grid.cols,
                rows: grid.rows,
                viewport_x_q: ox.round() as i32,
                viewport_y_q: oy.round() as i32,
                scroll_offset: grid.scroll_offset,
                is_alt_screen: grid.is_alt_screen,
                cursor_row: grid.cursor_row,
                cursor_col: grid.cursor_col,
                cursor_style: grid.cursor_style,
                show_cursor,
                selection: grid.selection,
                row_starts,
                has_overflow: overflow_instances.len() > pane_overflow_start,
            });
        }

        // Append wide-overflow glyphs (e.g. Nerd Font icons) last so they
        // render on top of neighboring cell backgrounds and aren't clipped.
        instances.append(&mut overflow_instances);

        // Upload atlas if new glyphs were rasterized
        if self.glyph_cache.has_dirty_pages() {
            self.upload_atlas_pages();
        }

        self.instance_count = instances.len() as u32;
        self.upload_instances_diff(&instances);

        // Save state for the next frame's partial-rebuild eligibility
        // check.  Any caller-side mutation that invalidates this cache
        // (resize, theme change, font change) just leaves the snapshot
        // pointing at stale offsets — the per-pane signature compare in
        // `try_update_multi_pane_partial` will mismatch and we'll fall
        // back to the full path on the very next frame, then refresh
        // here.  No eager invalidation needed.
        self.prev_pane_states = new_pane_states;
        self.prev_log_mode = log_mode;
        self.prev_find_all = find_highlights.clone();
        self.prev_find_cur = find_current.clone();
        self.prev_folds = folded_blocks.clone();
        self.prev_overflow_len = instances.len().saturating_sub(
            self.prev_pane_states
                .last()
                .and_then(|p| p.row_starts.last().copied())
                .unwrap_or(0),
        );

        // Return the backing storage so next frame can reuse the
        // pre-grown allocation.  `clear()` above leaves capacity intact.
        self.instance_scratch = instances;
        self.overflow_scratch = overflow_instances;
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
                let (atlas_rect, glyph_offset, atlas_idx) = if ch_char > ' ' {
                    if let Some(info) = self.glyph_cache.get_or_insert(ch_char) {
                        (
                            [
                                info.atlas_x as f32,
                                info.atlas_y as f32,
                                info.width as f32,
                                info.height as f32,
                            ],
                            [info.offset_x, info.offset_y],
                            info.atlas_idx,
                        )
                    } else {
                        ([0.0; 4], [0.0; 2], 0)
                    }
                } else {
                    ([0.0; 4], [0.0; 2], 0)
                };

                instances.push(CellInstance {
                    pos: [col_px, y_offset],
                    atlas_rect,
                    glyph_offset,
                    fg: tab_fg,
                    bg,
                    cell_span: 1.0,
                    _pad: atlas_idx as f32,
                });
                x_pos += 1.0;
            }
            x_pos += 1.0; // gap between tabs
        }

        // Upload atlas if needed
        if self.glyph_cache.has_dirty_pages() {
            self.upload_atlas_pages();
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
    pub fn handle_egui_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) -> bool {
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
        self.egui_pass_active = true;
    }

    /// `true` once we've rendered at least one egui frame end-to-end, so
    /// the caller knows it's safe to reuse `cached_paint_jobs` on the next
    /// render instead of running the full panel / widget tree again.
    /// Returns `false` before the very first render (cache empty) — the
    /// caller must run a full egui pass in that case.
    pub fn has_cached_egui_frame(&self) -> bool {
        !self.cached_paint_jobs.is_empty()
    }

    /// End the egui frame and stash its tessellated primitives into
    /// `self.cached_paint_jobs` so `render_frame` can pick them up
    /// without cloning.  Returns only the `TexturesDelta` — callers
    /// should read `&self.cached_paint_jobs` for the geometry.
    fn end_egui_frame(
        &mut self,
        window: &winit::window::Window,
    ) -> egui::TexturesDelta {
        let output = self.egui_ctx.end_pass();
        self.egui_winit
            .handle_platform_output(window, output.platform_output);

        // Capture egui's "render again in X" hint. Hover transitions, button
        // press animations, and tooltip fades all rely on this delay being
        // honoured by the host event loop.
        self.egui_repaint_after = output
            .viewport_output
            .iter()
            .map(|(_, vp)| vp.repaint_delay)
            .min()
            .unwrap_or(std::time::Duration::MAX);

        // Write primitives straight into the cache so the next skipped
        // frame (and this frame's upcoming render pass) can borrow them
        // without any deep clone.  A maximized window's UI tessellation
        // can hit multi-MB of vertex/index data; cloning it per frame
        // dominated the "drag while maximized" cost.
        self.cached_paint_jobs = self
            .egui_ctx
            .tessellate(output.shapes, output.pixels_per_point);
        self.egui_pass_active = false;
        output.textures_delta
    }

    /// How long until egui wants another frame. `Duration::MAX` means egui is
    /// idle and the main loop is free to sleep until external input arrives.
    pub fn egui_repaint_after(&self) -> std::time::Duration {
        self.egui_repaint_after
    }

    pub fn render_frame(&mut self, window: &winit::window::Window) -> Result<()> {
        // End egui frame and collect paint data.  When the caller chose
        // to skip the egui rebuild this frame (high-frequency drag
        // coalescing), `egui_pass_active` is `false` and we reuse the
        // cached primitives from the last non-skipped frame.  An empty
        // `TexturesDelta` is correct for skipped frames — nothing was
        // uploaded or freed, the GPU already has every font/image
        // texture from the previous render, and the cached `TextureId`s
        // reference them directly.
        let textures_delta = if self.egui_pass_active {
            self.end_egui_frame(window)
        } else {
            egui::TexturesDelta::default()
        };
        // Move the cached primitives out of `self` for the duration of
        // `render_frame` so we can keep a `&mut self` call chain (atlas
        // upload, queue submit, egui_renderer mut access) without the
        // borrow checker forbidding a concurrent `&self.cached_paint_jobs`
        // immutable borrow.  This is a pointer swap — no vertex/index
        // Vec data is copied, so it costs nothing on a skipped frame.
        // Restored at function exit; the only path that could drop it
        // is a panic, in which case losing the cache just forces a
        // single full egui rebuild on the next frame.
        let paint_jobs = std::mem::take(&mut self.cached_paint_jobs);

        let output = match self.surface.get_current_texture() {
            Ok(tex) => tex,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.surface_config);
                // Restore the cache even on the "recover and skip" path —
                // the next frame should still be able to take the skip
                // branch without redoing the whole UI tessellation.
                self.cached_paint_jobs = paint_jobs;
                return Ok(());
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                self.cached_paint_jobs = paint_jobs;
                anyhow::bail!("GPU out of memory");
            }
            Err(wgpu::SurfaceError::Timeout) => {
                warn!("surface acquire timeout, skipping frame");
                self.cached_paint_jobs = paint_jobs;
                return Ok(());
            }
            Err(e) => {
                self.cached_paint_jobs = paint_jobs;
                anyhow::bail!("surface error: {e}");
            }
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
                pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                let instances = &self.prev_instances[..self.instance_count as usize];
                let mut start = 0usize;
                while start < instances.len() {
                    let atlas_idx = instances[start]._pad as usize;
                    let mut end = start + 1;
                    while end < instances.len() && instances[end]._pad as usize == atlas_idx {
                        end += 1;
                    }
                    let page_idx = atlas_idx.min(self.atlas_pages.len().saturating_sub(1));
                    pass.set_bind_group(0, &self.atlas_pages[page_idx].bind_group, &[]);
                    pass.draw(0..6, start as u32..end as u32);
                    start = end;
                }
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
        let mut egui_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("egui-encoder"),
                });

        let egui_rend = self.egui_renderer.as_mut().expect("egui renderer missing");

        for (id, delta) in &textures_delta.set {
            egui_rend.update_texture(&self.device, &self.queue, *id, delta);
        }

        egui_rend.update_buffers(
            &self.device,
            &self.queue,
            &mut egui_encoder,
            &paint_jobs,
            &screen,
        );

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
        // Restore cache for the next frame's skip branch.  See the
        // `mem::take` comment up top — this is a pointer swap, free.
        self.cached_paint_jobs = paint_jobs;
        Ok(())
    }

    // ---- Internal helpers ----

    fn upload_atlas_pages(&mut self) {
        self.ensure_gpu_atlas_pages();
        let aw = self.glyph_cache.atlas_width;
        let ah = self.glyph_cache.atlas_height;
        for (idx, page) in self.glyph_cache.pages.iter_mut().enumerate() {
            let y_min = page.dirty_y_min;
            let y_max = page.dirty_y_max.min(ah);
            if !page.dirty || y_min >= y_max {
                continue;
            }

            let offset = (y_min * aw) as usize;
            let size = ((y_max - y_min) * aw) as usize;
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas_pages[idx].texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: y_min,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &page.data[offset..offset + size],
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

            page.dirty = false;
            page.dirty_y_min = u32::MAX;
            page.dirty_y_max = 0;
        }
    }

    fn ensure_gpu_atlas_pages(&mut self) {
        while self.atlas_pages.len() < self.glyph_cache.pages.len() {
            let idx = self.atlas_pages.len();
            let page = create_gpu_atlas_page(
                &self.device,
                &self.queue,
                &self.cell_pipeline,
                &self.uniform_buffer,
                &self.glyph_cache,
                idx,
            );
            self.atlas_pages.push(page);
        }
    }

    /// Upload `new` to the instance buffer while minimising PCIe traffic.
    ///
    /// Compares to `self.prev_instances` (last frame's snapshot) and transfers
    /// only the contiguous range spanning all changed instances. For steady
    /// state scenes (idle terminal, slow typing, cursor blink), this typically
    /// reduces the upload from hundreds of KB to a few cache lines.
    ///
    /// When the length shrinks, the tail is left untouched on the GPU but
    /// `instance_count` bounds the draw call so stale bytes aren't visible.
    fn upload_instances_diff(&mut self, new: &[CellInstance]) {
        use std::mem::size_of;
        let stride = size_of::<CellInstance>();

        // Fast path: first frame or length changed drastically — full upload.
        if self.prev_instances.len() != new.len() {
            if !new.is_empty() {
                self.queue
                    .write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(new));
            }
            self.prev_instances.clear();
            self.prev_instances.extend_from_slice(new);
            return;
        }

        // Byte-wise compare to find the minimal dirty window.
        let new_bytes: &[u8] = bytemuck::cast_slice(new);
        let prev_bytes: &[u8] = bytemuck::cast_slice(&self.prev_instances);
        let mut first = 0usize;
        while first < new.len()
            && new_bytes[first * stride..(first + 1) * stride]
                == prev_bytes[first * stride..(first + 1) * stride]
        {
            first += 1;
        }
        if first == new.len() {
            // Nothing changed — skip the upload entirely.
            return;
        }
        let mut last = new.len();
        while last > first
            && new_bytes[(last - 1) * stride..last * stride]
                == prev_bytes[(last - 1) * stride..last * stride]
        {
            last -= 1;
        }

        let offset = (first * stride) as u64;
        self.queue.write_buffer(
            &self.instance_buffer,
            offset,
            bytemuck::cast_slice(&new[first..last]),
        );

        // Refresh only the changed rows of the CPU snapshot.
        self.prev_instances[first..last].copy_from_slice(&new[first..last]);
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
