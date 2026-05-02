//! Glyph atlas: rasterizes glyphs via cosmic-text/swash and packs them into an R8 GPU texture.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent};
use std::collections::HashMap;
use tracing::info;

/// Cached information about a rasterized glyph in the atlas.
#[derive(Debug, Clone, Copy)]
pub struct GlyphInfo {
    /// Atlas page containing this glyph.
    pub atlas_idx: u16,
    /// Top-left position in the atlas texture (texels).
    pub atlas_x: u32,
    pub atlas_y: u32,
    /// Bitmap size (texels).
    pub width: u32,
    pub height: u32,
    /// Offset from the cell's top-left to the glyph bitmap's top-left (pixels).
    pub offset_x: f32,
    pub offset_y: f32,
}

/// One CPU-side atlas page and its row-packing state.
pub struct AtlasPage {
    /// R8 pixel data (one byte per texel = coverage alpha).
    pub data: Vec<u8>,
    /// True when `data` has been modified since the last GPU upload.
    pub dirty: bool,
    /// Dirty region (Y range in texel rows) for incremental upload.
    pub dirty_y_min: u32,
    pub dirty_y_max: u32,
    pack_x: u32,
    pack_y: u32,
    row_height: u32,
}

impl AtlasPage {
    fn new(width: u32, height: u32) -> Self {
        Self {
            data: vec![0u8; (width * height) as usize],
            dirty: true,
            dirty_y_min: 0,
            dirty_y_max: height,
            pack_x: 0,
            pack_y: 0,
            row_height: 0,
        }
    }
}

/// CPU-side glyph cache backed by a cosmic-text font system.
///
/// Rasterizes glyphs on demand and packs them into an R8-alpha atlas texture.
pub struct GlyphCache {
    font_system: FontSystem,
    swash_cache: SwashCache,

    // Atlas packing state
    pub atlas_width: u32,
    pub atlas_height: u32,
    pub pages: Vec<AtlasPage>,

    /// `char → Option<GlyphInfo>`. `None` means the char was tried but has no glyph.
    entries: HashMap<char, Option<GlyphInfo>>,

    // Font metrics
    pub cell_width: f32,
    pub cell_height: f32,
    font_size: f32,
    line_height: f32,
    font_family: String,
    /// Distance from cell top to the canonical baseline, in pixels.
    /// All rasterized glyphs (including fallback CJK fonts) have their
    /// `offset_y` computed against this value so they share a baseline.
    primary_ascent_px: f32,
}

impl GlyphCache {
    /// Create a glyph cache, measure cell dimensions, and pre-cache printable ASCII.
    /// `font_family`: e.g. "FiraCode Nerd Font", "Cascadia Code", or "" for system monospace.
    pub fn new(font_size: f32, font_family: &str) -> Self {
        let mut font_system = FontSystem::new();
        // Register bundled JetBrains Mono Nerd Font so it's always available.
        static JBMONO_REGULAR: &[u8] = include_bytes!("../fonts/JetBrainsMonoNerdFont-Regular.ttf");
        static JBMONO_BOLD: &[u8] = include_bytes!("../fonts/JetBrainsMonoNerdFont-Bold.ttf");
        font_system.db_mut().load_font_data(JBMONO_REGULAR.to_vec());
        font_system.db_mut().load_font_data(JBMONO_BOLD.to_vec());
        let swash_cache = SwashCache::new();

        // ── Read primary-font metrics directly from the TTF ──
        // Cell height = ascent + descent + leading, all derived from the regular
        // font's actual metrics so every glyph (including fallback CJK) can be
        // baseline-aligned consistently.
        let (line_height, primary_ascent_px) = primary_font_metrics(JBMONO_REGULAR, font_size)
            .unwrap_or_else(|| {
                // Fallback to the old heuristic if metric parsing fails.
                let h = (font_size * 1.5).ceil() + 2.0;
                (h, (font_size * 1.1).round())
            });

        let family = if font_family.is_empty() {
            Family::Name("JetBrainsMono Nerd Font")
        } else {
            Family::Name(font_family)
        };

        // Measure cell width by shaping 'M'
        let metrics = Metrics::new(font_size, line_height);
        let mut buf = Buffer::new(&mut font_system, metrics);
        buf.set_size(&mut font_system, Some(500.0), Some(line_height + 10.0));
        buf.set_text(
            &mut font_system,
            "M",
            Attrs::new().family(family),
            Shaping::Advanced,
        );
        buf.shape_until_scroll(&mut font_system, false);

        let cell_width = buf
            .layout_runs()
            .next()
            .and_then(|r| r.glyphs.first())
            .map(|g| g.w)
            .unwrap_or(font_size * 0.6);

        let atlas_width = 4096u32;
        let atlas_height = 4096u32;

        let mut cache = Self {
            font_system,
            swash_cache,
            atlas_width,
            atlas_height,
            pages: vec![AtlasPage::new(atlas_width, atlas_height)],
            entries: HashMap::with_capacity(256),
            cell_width,
            cell_height: line_height,
            font_size,
            line_height,
            font_family: font_family.to_string(),
            primary_ascent_px,
        };

        // Pre-cache printable ASCII (32..=126)
        for c in ' '..='~' {
            cache.get_or_insert(c);
        }

        info!(
            cell_w = cell_width,
            cell_h = line_height,
            ascent = primary_ascent_px,
            glyphs_cached = cache.entries.len(),
            "glyph cache initialized"
        );
        cache
    }

    /// Look up (or rasterize and insert) a glyph for the given character.
    pub fn get_or_insert(&mut self, c: char) -> Option<&GlyphInfo> {
        if !self.entries.contains_key(&c) {
            let info = self.rasterize_and_pack(c);
            self.entries.insert(c, info);
        }
        self.entries.get(&c).and_then(|opt| opt.as_ref())
    }

    /// Shape, rasterize, and pack a single character into the atlas.
    fn rasterize_and_pack(&mut self, c: char) -> Option<GlyphInfo> {
        if c == ' ' {
            return None; // no glyph needed for space
        }

        // ── Fast path: built-in box-drawing and block-element rendering ──
        // These characters are drawn pixel-perfectly at exact cell dimensions
        // so neighbouring cells connect seamlessly (fonts give sub-pixel gaps).
        if crate::builtin_font::is_builtin(c) {
            let cw = self.cell_width.round().max(1.0) as u32;
            let ch = self.cell_height.round().max(1.0) as u32;
            if let Some(bg) = crate::builtin_font::builtin_glyph(c, cw, ch) {
                return self.pack_r8(bg.width, bg.height, bg.offset_x, bg.offset_y, &bg.data);
            }
        }

        let metrics = Metrics::new(self.font_size, self.line_height);
        let mut buf = Buffer::new(&mut self.font_system, metrics);
        // Use generous buffer width AND height so wide/tall glyphs (Nerd Font PUA
        // icons, accented chars, CJK radicals) aren't clipped during shaping.
        // The +16 vertical slack is essential for tall icons; the resulting
        // line_y already starts at the top, so no centering offset is added.
        buf.set_size(
            &mut self.font_system,
            Some(self.cell_width * 4.0),
            Some(self.cell_height + 16.0),
        );

        let s = c.to_string();
        let family = if self.font_family.is_empty() {
            Family::Name("JetBrainsMono Nerd Font")
        } else {
            Family::Name(&self.font_family)
        };
        buf.set_text(
            &mut self.font_system,
            &s,
            Attrs::new().family(family),
            Shaping::Advanced,
        );
        buf.shape_until_scroll(&mut self.font_system, false);

        let run = buf.layout_runs().next()?;
        let glyph = run.glyphs.first()?;
        let physical = glyph.physical((0.0, 0.0), 1.0);

        let image = self
            .swash_cache
            .get_image_uncached(&mut self.font_system, physical.cache_key)?;

        // Accept alpha-mask and subpixel-mask glyphs (covers most Nerd Font icons)
        match image.content {
            SwashContent::Mask | SwashContent::SubpixelMask => {}
            _ => return None,
        }

        let w = image.placement.width as u32;
        let h = image.placement.height as u32;
        if w == 0 || h == 0 {
            return None;
        }

        let (atlas_idx, ax, ay) = self.allocate_atlas_rect(w, h, c)?;
        let page = &mut self.pages[atlas_idx as usize];

        // Copy bitmap rows into the atlas
        let is_subpixel = image.content == SwashContent::SubpixelMask;
        for row in 0..h {
            let dst_start = ((ay + row) * self.atlas_width + ax) as usize;
            if is_subpixel {
                // SubpixelMask: 3 bytes per pixel (R, G, B). Average to single alpha.
                let src_start = (row * w * 3) as usize;
                for col in 0..w as usize {
                    let r = image.data[src_start + col * 3] as u16;
                    let g = image.data[src_start + col * 3 + 1] as u16;
                    let b = image.data[src_start + col * 3 + 2] as u16;
                    page.data[dst_start + col] = ((r + g + b) / 3) as u8;
                }
            } else {
                // Mask: 1 byte per pixel
                let src_start = (row * w) as usize;
                page.data[dst_start..dst_start + w as usize]
                    .copy_from_slice(&image.data[src_start..src_start + w as usize]);
            }
        }

        page.dirty = true;
        page.dirty_y_min = page.dirty_y_min.min(ay);
        page.dirty_y_max = page.dirty_y_max.max(ay + h);

        // Glyph offsets MUST be integer-valued so that fragment coords land on
        // texel centres. We anchor every glyph to the canonical baseline
        // (`primary_ascent_px` from cell top), regardless of which font the
        // shaper actually picked — this keeps ASCII and CJK on the same line.
        let offset_x = (physical.x as f32 + image.placement.left as f32).round();
        let offset_y =
            (self.primary_ascent_px + physical.y as f32 - image.placement.top as f32).round();

        Some(GlyphInfo {
            atlas_idx,
            atlas_x: ax,
            atlas_y: ay,
            width: w,
            height: h,
            offset_x,
            offset_y,
        })
    }

    /// Pack a pre-rasterized R8 bitmap (one alpha byte per pixel, row-major)
    /// into the atlas. Returns the resulting [`GlyphInfo`].
    fn pack_r8(
        &mut self,
        w: u32,
        h: u32,
        offset_x: f32,
        offset_y: f32,
        pixels: &[u8],
    ) -> Option<GlyphInfo> {
        if w == 0 || h == 0 || pixels.len() < (w * h) as usize {
            return None;
        }

        let (atlas_idx, ax, ay) = self.allocate_atlas_rect(w, h, '\0')?;
        let page = &mut self.pages[atlas_idx as usize];

        for row in 0..h {
            let dst_start = ((ay + row) * self.atlas_width + ax) as usize;
            let src_start = (row * w) as usize;
            page.data[dst_start..dst_start + w as usize]
                .copy_from_slice(&pixels[src_start..src_start + w as usize]);
        }

        page.dirty = true;
        page.dirty_y_min = page.dirty_y_min.min(ay);
        page.dirty_y_max = page.dirty_y_max.max(ay + h);

        Some(GlyphInfo {
            atlas_idx,
            atlas_x: ax,
            atlas_y: ay,
            width: w,
            height: h,
            offset_x,
            offset_y,
        })
    }

    fn allocate_atlas_rect(&mut self, w: u32, h: u32, c: char) -> Option<(u16, u32, u32)> {
        if w > self.atlas_width || h > self.atlas_height {
            if c == '\0' {
                tracing::warn!("glyph atlas cannot fit built-in glyph ({}x{})", w, h);
            } else {
                tracing::warn!("glyph atlas cannot fit '{c}' ({}x{})", w, h);
            }
            return None;
        }

        for (idx, page) in self.pages.iter_mut().enumerate() {
            if page.pack_x + w > self.atlas_width {
                page.pack_x = 0;
                page.pack_y += page.row_height + 1;
                page.row_height = 0;
            }
            if page.pack_y + h <= self.atlas_height {
                let ax = page.pack_x;
                let ay = page.pack_y;
                page.pack_x += w + 1;
                page.row_height = page.row_height.max(h);
                return Some((idx as u16, ax, ay));
            }
        }

        let atlas_idx = self.pages.len();
        if atlas_idx > u16::MAX as usize {
            tracing::warn!("glyph atlas page limit reached");
            return None;
        }
        tracing::info!(atlas_idx, "glyph atlas full, adding page");
        let mut page = AtlasPage::new(self.atlas_width, self.atlas_height);
        let ax = page.pack_x;
        let ay = page.pack_y;
        page.pack_x += w + 1;
        page.row_height = h;
        self.pages.push(page);
        Some((atlas_idx as u16, ax, ay))
    }

    pub fn has_dirty_pages(&self) -> bool {
        self.pages.iter().any(|page| page.dirty)
    }
}

/// Read the primary font's metrics directly from its TTF bytes and convert to
/// pixel-space at `font_size`. Returns `(cell_height, ascent_px)` where
/// `cell_height = ascent + descent + leading` and `ascent_px` is the canonical
/// distance from the cell top to the baseline.
///
/// All values are rounded so that integer pixel offsets line up with texel
/// centres in the atlas.
fn primary_font_metrics(font_bytes: &[u8], font_size: f32) -> Option<(f32, f32)> {
    let font = swash::FontRef::from_index(font_bytes, 0)?;
    let m = font.metrics(&[]);
    let upem = m.units_per_em as f32;
    if upem <= 0.0 {
        return None;
    }
    let scale = font_size / upem;
    let ascent = (m.ascent * scale).round();
    let descent = (m.descent * scale).round();
    let leading = (m.leading * scale).round();
    // Add a 1-pixel safety pad so descenders don't get clipped on rounding.
    let cell_height = (ascent + descent + leading + 1.0).max(font_size);
    Some((cell_height, ascent))
}
