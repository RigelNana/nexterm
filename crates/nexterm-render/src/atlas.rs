//! Glyph atlas: rasterizes glyphs via cosmic-text/swash and packs them into an R8 GPU texture.

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent,
};
use std::collections::HashMap;
use tracing::info;

/// Cached information about a rasterized glyph in the atlas.
#[derive(Debug, Clone, Copy)]
pub struct GlyphInfo {
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

/// CPU-side glyph cache backed by a cosmic-text font system.
///
/// Rasterizes glyphs on demand and packs them into an R8-alpha atlas texture.
pub struct GlyphCache {
    font_system: FontSystem,
    swash_cache: SwashCache,

    // Atlas packing state
    pub atlas_width: u32,
    pub atlas_height: u32,
    /// R8 pixel data (one byte per texel = coverage alpha).
    pub atlas_data: Vec<u8>,
    /// True when `atlas_data` has been modified since the last GPU upload.
    pub atlas_dirty: bool,
    /// Dirty region (Y range in texel rows) for incremental upload.
    pub atlas_dirty_y_min: u32,
    pub atlas_dirty_y_max: u32,
    pack_x: u32,
    pack_y: u32,
    row_height: u32,

    /// `char → Option<GlyphInfo>`. `None` means the char was tried but has no glyph.
    entries: HashMap<char, Option<GlyphInfo>>,

    // Font metrics
    pub cell_width: f32,
    pub cell_height: f32,
    font_size: f32,
    line_height: f32,
    font_family: String,
}

impl GlyphCache {
    /// Create a glyph cache, measure cell dimensions, and pre-cache printable ASCII.
    /// `font_family`: e.g. "FiraCode Nerd Font", "Cascadia Code", or "" for system monospace.
    pub fn new(font_size: f32, font_family: &str) -> Self {
        // 1.5x font_size + 2px safety pad gives CJK glyphs and accented
        // characters enough vertical room to avoid top/bottom clipping after
        // rounding the integer-aligned glyph offsets.
        let line_height = (font_size * 1.5).ceil() + 2.0;
        let mut font_system = FontSystem::new();
        // Register bundled JetBrains Mono Nerd Font so it's always available.
        static JBMONO_REGULAR: &[u8] = include_bytes!("../fonts/JetBrainsMonoNerdFont-Regular.ttf");
        static JBMONO_BOLD: &[u8] = include_bytes!("../fonts/JetBrainsMonoNerdFont-Bold.ttf");
        font_system.db_mut().load_font_data(JBMONO_REGULAR.to_vec());
        font_system.db_mut().load_font_data(JBMONO_BOLD.to_vec());
        let swash_cache = SwashCache::new();

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
            atlas_data: vec![0u8; (atlas_width * atlas_height) as usize],
            atlas_dirty: true,
            atlas_dirty_y_min: u32::MAX,
            atlas_dirty_y_max: 0,
            pack_x: 0,
            pack_y: 0,
            row_height: 0,
            entries: HashMap::with_capacity(256),
            cell_width,
            cell_height: line_height,
            font_size,
            line_height,
            font_family: font_family.to_string(),
        };

        // Pre-cache printable ASCII (32..=126)
        for c in ' '..='~' {
            cache.get_or_insert(c);
        }

        info!(
            cell_w = cell_width,
            cell_h = line_height,
            glyphs_cached = cache.entries.len(),
            "glyph cache initialized"
        );
        cache
    }

    /// Clear the atlas, all cached entries, and packing state. Caller must
    /// re-rasterize glyphs as needed and re-upload the atlas to the GPU.
    fn reset_atlas(&mut self) {
        self.atlas_data.fill(0);
        self.pack_x = 0;
        self.pack_y = 0;
        self.row_height = 0;
        self.entries.clear();
        self.atlas_dirty = true;
        self.atlas_dirty_y_min = 0;
        self.atlas_dirty_y_max = self.atlas_height;
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
        let line_y = run.line_y; // baseline Y = line_top + centering + ascent
        let glyph = run.glyphs.first()?;
        let physical = glyph.physical((0.0, 0.0), 1.0);

        let image = self.swash_cache.get_image_uncached(
            &mut self.font_system,
            physical.cache_key,
        )?;

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

        // Pack into atlas (row-based bin packing)
        if self.pack_x + w > self.atlas_width {
            self.pack_x = 0;
            self.pack_y += self.row_height + 1; // 1px padding
            self.row_height = 0;
        }
        if self.pack_y + h > self.atlas_height {
            // Atlas full: clear and restart. Existing glyph cache entries are
            // wiped so they'll re-rasterize on next access.
            tracing::info!("glyph atlas full, clearing and rebuilding");
            self.reset_atlas();
        }
        if self.pack_y + h > self.atlas_height {
            tracing::warn!("glyph atlas cannot fit '{c}' (h={h}) even after reset");
            return None;
        }

        let ax = self.pack_x;
        let ay = self.pack_y;

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
                    self.atlas_data[dst_start + col] = ((r + g + b) / 3) as u8;
                }
            } else {
                // Mask: 1 byte per pixel
                let src_start = (row * w) as usize;
                self.atlas_data[dst_start..dst_start + w as usize]
                    .copy_from_slice(&image.data[src_start..src_start + w as usize]);
            }
        }

        self.pack_x += w + 1; // 1px padding
        self.row_height = self.row_height.max(h);
        self.atlas_dirty = true;
        self.atlas_dirty_y_min = self.atlas_dirty_y_min.min(ay);
        self.atlas_dirty_y_max = self.atlas_dirty_y_max.max(ay + h);

        // Glyph offsets MUST be integer-valued so that fragment coords land on
        // texel centers. line_y is fractional in cosmic-text, so round here.
        let offset_x = (physical.x as f32 + image.placement.left as f32).round();
        let offset_y = (line_y + physical.y as f32 - image.placement.top as f32).round();

        Some(GlyphInfo {
            atlas_x: ax,
            atlas_y: ay,
            width: w,
            height: h,
            offset_x,
            offset_y,
        })
    }
}
