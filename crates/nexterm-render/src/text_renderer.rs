//! CPU-side text renderer: rasterizes the terminal grid into an RGBA pixel buffer
//! using cosmic-text for font shaping and swash for glyph rasterization.

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent,
};
use nexterm_vte::grid::{Color, Grid};

/// ANSI 16-color palette (Catppuccin Mocha inspired).
const ANSI_COLORS: [[u8; 3]; 16] = [
    [69, 71, 90],     // 0  black   (surface1)
    [243, 139, 168],  // 1  red
    [166, 227, 161],  // 2  green
    [249, 226, 175],  // 3  yellow
    [137, 180, 250],  // 4  blue
    [245, 194, 231],  // 5  magenta
    [148, 226, 213],  // 6  cyan
    [186, 194, 222],  // 7  white   (subtext1)
    [88, 91, 112],    // 8  bright black  (surface2)
    [243, 139, 168],  // 9  bright red
    [166, 227, 161],  // 10 bright green
    [249, 226, 175],  // 11 bright yellow
    [137, 180, 250],  // 12 bright blue
    [245, 194, 231],  // 13 bright magenta
    [148, 226, 213],  // 14 bright cyan
    [205, 214, 244],  // 15 bright white (text)
];

/// Default foreground / background as RGB.
const DEFAULT_FG: [u8; 3] = [205, 214, 244]; // #cdd6f4
const DEFAULT_BG: [u8; 3] = [30, 30, 46];    // #1e1e2e

/// Renders a `Grid` to an RGBA pixel buffer using cosmic-text.
pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    /// Measured monospace cell width in pixels.
    pub cell_width: f32,
    /// Measured cell height (= line height) in pixels.
    pub cell_height: f32,
    font_size: f32,
}

impl TextRenderer {
    /// Create a new text renderer with the given font size.
    pub fn new(font_size: f32) -> Self {
        let line_height = (font_size * 1.4).ceil();
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();

        // Measure cell width by shaping a single 'M'
        let metrics = Metrics::new(font_size, line_height);
        let mut measure = Buffer::new(&mut font_system, metrics);
        measure.set_size(&mut font_system, Some(500.0), Some(line_height + 10.0));
        measure.set_text(
            &mut font_system,
            "M",
            Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
        );
        measure.shape_until_scroll(&mut font_system, false);

        let cell_width = measure
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first())
            .map(|g| g.w)
            .unwrap_or(font_size * 0.6);

        Self {
            font_system,
            swash_cache,
            cell_width,
            cell_height: line_height,
            font_size,
        }
    }

    /// Render the terminal grid into an RGBA pixel buffer.
    ///
    /// The buffer is resized to `buf_width × buf_height × 4` bytes.
    pub fn render_grid(
        &mut self,
        grid: &Grid,
        pixel_buf: &mut Vec<u8>,
        buf_width: u32,
        buf_height: u32,
    ) {
        let total = (buf_width as usize) * (buf_height as usize) * 4;
        pixel_buf.resize(total, 0);

        // 1. Fast fill entire buffer with default background
        let bg_pixel = [DEFAULT_BG[0], DEFAULT_BG[1], DEFAULT_BG[2], 255u8];
        for chunk in pixel_buf.chunks_exact_mut(4) {
            chunk.copy_from_slice(&bg_pixel);
        }

        // 2. Paint non-default cell backgrounds only
        let stride = (buf_width * 4) as usize;
        for row_idx in 0..grid.rows {
            let y0 = (row_idx as f32 * self.cell_height) as u32;
            let y1 = (((row_idx + 1) as f32) * self.cell_height) as u32;
            for col_idx in 0..grid.cols {
                let cell = &grid.cells[row_idx][col_idx];
                if matches!(cell.bg, Color::Default) {
                    continue; // already filled
                }
                let bg = color_to_rgb(&cell.bg, false);
                let bg_px = [bg[0], bg[1], bg[2], 255u8];
                let x0 = (col_idx as f32 * self.cell_width) as u32;
                let x1 = (((col_idx + 1) as f32) * self.cell_width) as u32;
                for py in y0..y1.min(buf_height) {
                    let row_start = py as usize * stride;
                    for px in x0..x1.min(buf_width) {
                        let idx = row_start + px as usize * 4;
                        if idx + 3 < total {
                            pixel_buf[idx..idx + 4].copy_from_slice(&bg_px);
                        }
                    }
                }
            }
        }

        // 3. Render text via cosmic-text — skip empty rows
        let metrics = Metrics::new(self.font_size, self.cell_height);
        for row_idx in 0..grid.rows {
            // Build row text and trim trailing spaces
            let row_text: String = grid.cells[row_idx].iter().map(|c| c.ch).collect();
            let trimmed = row_text.trim_end();
            if trimmed.is_empty() {
                continue; // skip blank rows entirely
            }
            let y_offset = (row_idx as f32 * self.cell_height) as i32;

            let mut buffer = Buffer::new(&mut self.font_system, metrics);
            buffer.set_size(
                &mut self.font_system,
                Some(buf_width as f32),
                Some(self.cell_height + 4.0),
            );
            buffer.set_text(
                &mut self.font_system,
                trimmed,
                Attrs::new().family(Family::Monospace),
                Shaping::Advanced,
            );
            buffer.shape_until_scroll(&mut self.font_system, false);

            for run in buffer.layout_runs() {
                for glyph in run.glyphs.iter() {
                    let col_idx = (glyph.x / self.cell_width) as usize;
                    let fg = if col_idx < grid.cols {
                        color_to_rgb(&grid.cells[row_idx][col_idx].fg, true)
                    } else {
                        DEFAULT_FG
                    };

                    let physical = glyph.physical((0.0, 0.0), 1.0);
                    let image = self.swash_cache.get_image_uncached(
                        &mut self.font_system,
                        physical.cache_key,
                    );
                    let image = match image {
                        Some(img) => img,
                        None => continue,
                    };

                    let gx = physical.x + image.placement.left;
                    let gy = y_offset + physical.y - image.placement.top;

                    for dy in 0..image.placement.height as i32 {
                        for dx in 0..image.placement.width as i32 {
                            let px = gx + dx;
                            let py = gy + dy;
                            if px < 0 || py < 0 {
                                continue;
                            }
                            let px = px as u32;
                            let py = py as u32;
                            if px >= buf_width || py >= buf_height {
                                continue;
                            }
                            let dst = ((py * buf_width + px) * 4) as usize;
                            if dst + 3 >= total {
                                continue;
                            }

                            match image.content {
                                SwashContent::Mask => {
                                    let src = (dy * image.placement.width as i32 + dx) as usize;
                                    if src >= image.data.len() {
                                        continue;
                                    }
                                    let alpha = image.data[src] as u32;
                                    if alpha == 0 {
                                        continue;
                                    }
                                    let inv = 255 - alpha;
                                    pixel_buf[dst] = ((fg[0] as u32 * alpha
                                        + pixel_buf[dst] as u32 * inv)
                                        / 255) as u8;
                                    pixel_buf[dst + 1] = ((fg[1] as u32 * alpha
                                        + pixel_buf[dst + 1] as u32 * inv)
                                        / 255) as u8;
                                    pixel_buf[dst + 2] = ((fg[2] as u32 * alpha
                                        + pixel_buf[dst + 2] as u32 * inv)
                                        / 255) as u8;
                                    pixel_buf[dst + 3] = 255;
                                }
                                SwashContent::Color => {
                                    let src =
                                        ((dy * image.placement.width as i32 + dx) * 4) as usize;
                                    if src + 3 < image.data.len() {
                                        pixel_buf[dst..dst + 4]
                                            .copy_from_slice(&image.data[src..src + 4]);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        // 3. Draw cursor (simple block cursor)
        let cx = (grid.cursor_col as f32 * self.cell_width) as u32;
        let cy = (grid.cursor_row as f32 * self.cell_height) as u32;
        let cw = self.cell_width as u32;
        let ch = self.cell_height as u32;
        for py in cy..((cy + ch).min(buf_height)) {
            for px in cx..((cx + cw).min(buf_width)) {
                let idx = ((py * buf_width + px) * 4) as usize;
                if idx + 3 < total {
                    // Semi-transparent white cursor
                    let alpha = 160u32;
                    let inv = 255 - alpha;
                    pixel_buf[idx] =
                        ((205u32 * alpha + pixel_buf[idx] as u32 * inv) / 255) as u8;
                    pixel_buf[idx + 1] =
                        ((214u32 * alpha + pixel_buf[idx + 1] as u32 * inv) / 255) as u8;
                    pixel_buf[idx + 2] =
                        ((244u32 * alpha + pixel_buf[idx + 2] as u32 * inv) / 255) as u8;
                    pixel_buf[idx + 3] = 255;
                }
            }
        }
    }
}

/// Convert a Grid `Color` to an `[r, g, b]` triple.
fn color_to_rgb(color: &Color, is_fg: bool) -> [u8; 3] {
    match color {
        Color::Default => {
            if is_fg {
                DEFAULT_FG
            } else {
                DEFAULT_BG
            }
        }
        Color::Indexed(idx) => {
            let i = *idx as usize;
            if i < 16 {
                ANSI_COLORS[i]
            } else if i < 232 {
                // 216-color cube
                let i = i - 16;
                let r = (i / 36) * 51;
                let g = ((i / 6) % 6) * 51;
                let b = (i % 6) * 51;
                [r as u8, g as u8, b as u8]
            } else {
                // Grayscale ramp
                let v = ((i - 232) * 10 + 8) as u8;
                [v, v, v]
            }
        }
        Color::Rgb(r, g, b) => [*r, *g, *b],
    }
}
