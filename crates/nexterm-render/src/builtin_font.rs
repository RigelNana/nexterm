//! Built-in pixel-perfect drawing for box-drawing and block-element characters.
//!
//! Fonts draw `─│╭╮╰╯█▀▄` etc. per-glyph, so adjacent cells often have
//! sub-pixel gaps or uneven alignment. This module bypasses the font entirely
//! for these ranges and rasterizes each glyph as a full `cell_width × cell_height`
//! bitmap, guaranteeing that neighbouring cells connect seamlessly.
//!
//! Port of Alacritty's `renderer/text/builtin_font.rs` adapted to nexterm's
//! single-channel (R8 alpha) atlas.

use std::cmp;

const COLOR_FILL: u8 = 255;
const COLOR_FILL_ALPHA_STEP_1: u8 = 192; // ▓
const COLOR_FILL_ALPHA_STEP_2: u8 = 128; // ▒
const COLOR_FILL_ALPHA_STEP_3: u8 = 64; // ░

/// A rasterized bitmap ready to be copied into the atlas.
#[derive(Debug, Clone)]
pub struct BuiltinGlyph {
    /// Row-major R8 pixel buffer of length `width * height`.
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Offset from the cell's top-left to the bitmap's top-left.
    pub offset_x: f32,
    pub offset_y: f32,
}

/// True if `c` belongs to a range nexterm renders internally.
#[inline]
pub fn is_builtin(c: char) -> bool {
    matches!(
        c,
        // Box Drawing (skip diagonals 2571..=2573, they need larger canvas)
        '\u{2500}'..='\u{2570}' | '\u{2574}'..='\u{257f}'
        // Block Elements
        | '\u{2580}'..='\u{259f}'
    )
}

/// Rasterize a built-in glyph at exact cell dimensions.
pub fn builtin_glyph(c: char, cell_width: u32, cell_height: u32) -> Option<BuiltinGlyph> {
    if !is_builtin(c) {
        return None;
    }
    let width = cell_width.max(1) as usize;
    let height = cell_height.max(1) as usize;
    let stroke_size = calculate_stroke_size(width);
    let heavy_stroke_size = stroke_size * 2;

    let mut canvas = Canvas::new(width, height);

    match c {
        // ── Horizontal dashes: '┄', '┅', '┈', '┉', '╌', '╍' ──
        '\u{2504}' | '\u{2505}' | '\u{2508}' | '\u{2509}' | '\u{254c}' | '\u{254d}' => {
            let (num_gaps, stroke_size) = match c {
                '\u{2504}' => (2, stroke_size),
                '\u{2505}' => (2, heavy_stroke_size),
                '\u{2508}' => (3, stroke_size),
                '\u{2509}' => (3, heavy_stroke_size),
                '\u{254c}' => (1, stroke_size),
                '\u{254d}' => (1, heavy_stroke_size),
                _ => unreachable!(),
            };
            let dash_gap_len = cmp::max(width / 8, 1);
            let dash_len = cmp::max(
                width.saturating_sub(dash_gap_len * num_gaps) / (num_gaps + 1),
                1,
            );
            let y = canvas.y_center();
            for gap in 0..=num_gaps {
                let x = cmp::min(gap * (dash_len + dash_gap_len), width);
                canvas.draw_h_line(x as f32, y, dash_len as f32, stroke_size);
            }
        }

        // ── Vertical dashes: '┆', '┇', '┊', '┋', '╎', '╏' ──
        '\u{2506}' | '\u{2507}' | '\u{250a}' | '\u{250b}' | '\u{254e}' | '\u{254f}' => {
            let (num_gaps, stroke_size) = match c {
                '\u{2506}' => (2, stroke_size),
                '\u{2507}' => (2, heavy_stroke_size),
                '\u{250a}' => (3, stroke_size),
                '\u{250b}' => (3, heavy_stroke_size),
                '\u{254e}' => (1, stroke_size),
                '\u{254f}' => (1, heavy_stroke_size),
                _ => unreachable!(),
            };
            let dash_gap_len = cmp::max(height / 8, 1);
            let dash_len = cmp::max(
                height.saturating_sub(dash_gap_len * num_gaps) / (num_gaps + 1),
                1,
            );
            let x = canvas.x_center();
            for gap in 0..=num_gaps {
                let y = cmp::min(gap * (dash_len + dash_gap_len), height);
                canvas.draw_v_line(x, y as f32, dash_len as f32, stroke_size);
            }
        }

        // ── Light/heavy lines and all box components (no diagonals) ──
        '\u{2500}'..='\u{2503}' | '\u{250c}'..='\u{254b}' | '\u{2574}'..='\u{257f}' => {
            let (sh1, sh2, sv1, sv2) = box_line_strokes(c, stroke_size, heavy_stroke_size);

            let x_v = canvas.x_center();
            let y_h = canvas.y_center();

            let v_top = canvas.v_line_bounds(x_v, sv1);
            let v_bot = canvas.v_line_bounds(x_v, sv2);
            let h_left = canvas.h_line_bounds(y_h, sh1);
            let h_right = canvas.h_line_bounds(y_h, sh2);

            let size_h1 = cmp::max(v_top.1 as i32, v_bot.1 as i32) as f32;
            let x_h = cmp::min(v_top.0 as i32, v_bot.0 as i32) as f32;
            let size_h2 = width as f32 - x_h;

            let size_v1 = cmp::max(h_left.1 as i32, h_right.1 as i32) as f32;
            let y_v = cmp::min(h_left.0 as i32, h_right.0 as i32) as f32;
            let size_v2 = height as f32 - y_v;

            canvas.draw_h_line(0.0, y_h, size_h1, sh1);
            canvas.draw_h_line(x_h, y_h, size_h2, sh2);
            canvas.draw_v_line(x_v, 0.0, size_v1, sv1);
            canvas.draw_v_line(x_v, y_v, size_v2, sv2);
        }

        // ── Double lines: '═','║','╒..╬' ──
        '\u{2550}'..='\u{256c}' => {
            draw_double_lines(&mut canvas, c, stroke_size);
        }

        // ── Rounded arcs: '╭', '╮', '╯', '╰' ──
        '\u{256d}' | '\u{256e}' | '\u{256f}' | '\u{2570}' => {
            canvas.draw_rounded_corner(stroke_size);

            // Mirror X for '╭' (256d) and '╰' (2570): these need the arc on the right.
            if c == '\u{256d}' || c == '\u{2570}' {
                canvas.flip_horizontal_with_stroke(stroke_size);
            }
            // Mirror Y for '╭' (256d) and '╮' (256e): these need the arc on the bottom.
            if c == '\u{256d}' || c == '\u{256e}' {
                canvas.flip_vertical_with_stroke(stroke_size);
            }
        }

        // ── Horizontal/vertical half-line helpers '╴','╵','╶','╷','╸','╹','╺','╻' etc. ──
        // Already covered by the light/heavy dispatch above.

        // ── Block element parts: half-blocks and column slices ──
        '\u{2580}'..='\u{2587}' | '\u{2589}'..='\u{2590}' | '\u{2594}' | '\u{2595}' => {
            let w = width as f32;
            let h = height as f32;
            let mut rect_w = match c {
                '\u{2589}' => w * 7.0 / 8.0,
                '\u{258a}' => w * 6.0 / 8.0,
                '\u{258b}' => w * 5.0 / 8.0,
                '\u{258c}' => w * 4.0 / 8.0,
                '\u{258d}' => w * 3.0 / 8.0,
                '\u{258e}' => w * 2.0 / 8.0,
                '\u{258f}' => w * 1.0 / 8.0,
                '\u{2590}' => w * 4.0 / 8.0,
                '\u{2595}' => w * 1.0 / 8.0,
                _ => w,
            };
            let (mut rect_h, mut y) = match c {
                '\u{2580}' => (h * 4.0 / 8.0, h * 8.0 / 8.0),
                '\u{2581}' => (h * 1.0 / 8.0, h * 1.0 / 8.0),
                '\u{2582}' => (h * 2.0 / 8.0, h * 2.0 / 8.0),
                '\u{2583}' => (h * 3.0 / 8.0, h * 3.0 / 8.0),
                '\u{2584}' => (h * 4.0 / 8.0, h * 4.0 / 8.0),
                '\u{2585}' => (h * 5.0 / 8.0, h * 5.0 / 8.0),
                '\u{2586}' => (h * 6.0 / 8.0, h * 6.0 / 8.0),
                '\u{2587}' => (h * 7.0 / 8.0, h * 7.0 / 8.0),
                '\u{2594}' => (h * 1.0 / 8.0, h * 8.0 / 8.0),
                _ => (h, h),
            };

            // Convert `y` from "size from bottom" to "y from top".
            y = (h - y).round();
            rect_w = rect_w.round().max(1.0);
            rect_h = rect_h.round().max(1.0);

            let x = match c {
                '\u{2590}' => canvas.x_center(),
                '\u{2595}' => w - rect_w,
                _ => 0.0,
            };

            canvas.draw_rect(x, y, rect_w, rect_h, COLOR_FILL);
        }

        // ── Full block and shades: '█', '░', '▒', '▓' ──
        '\u{2588}' => canvas.fill(COLOR_FILL),
        '\u{2591}' => canvas.fill(COLOR_FILL_ALPHA_STEP_3),
        '\u{2592}' => canvas.fill(COLOR_FILL_ALPHA_STEP_2),
        '\u{2593}' => canvas.fill(COLOR_FILL_ALPHA_STEP_1),

        // ── Quadrants: '▖', '▗', '▘', '▙', '▚', '▛', '▜', '▝', '▞', '▟' ──
        '\u{2596}'..='\u{259f}' => {
            let x_center = canvas.x_center().round().max(1.0);
            let y_center = canvas.y_center().round().max(1.0);

            // Second (top-left) quadrant.
            let (w2, h2) = match c {
                '\u{2598}' | '\u{2599}' | '\u{259a}' | '\u{259b}' | '\u{259c}' => {
                    (x_center, y_center)
                }
                _ => (0.0, 0.0),
            };
            // First (top-right) quadrant.
            let (w1, h1) = match c {
                '\u{259b}' | '\u{259c}' | '\u{259d}' | '\u{259e}' | '\u{259f}' => {
                    (x_center, y_center)
                }
                _ => (0.0, 0.0),
            };
            // Third (bottom-left) quadrant.
            let (w3, h3) = match c {
                '\u{2596}' | '\u{2599}' | '\u{259b}' | '\u{259e}' | '\u{259f}' => {
                    (x_center, y_center)
                }
                _ => (0.0, 0.0),
            };
            // Fourth (bottom-right) quadrant.
            let (w4, h4) = match c {
                '\u{2597}' | '\u{2599}' | '\u{259a}' | '\u{259c}' | '\u{259f}' => {
                    (x_center, y_center)
                }
                _ => (0.0, 0.0),
            };

            canvas.draw_rect(0.0, 0.0, w2, h2, COLOR_FILL);
            canvas.draw_rect(x_center, 0.0, w1, h1, COLOR_FILL);
            canvas.draw_rect(0.0, y_center, w3, h3, COLOR_FILL);
            canvas.draw_rect(x_center, y_center, w4, h4, COLOR_FILL);
        }

        _ => return None,
    }

    Some(BuiltinGlyph {
        data: canvas.buffer,
        width: width as u32,
        height: height as u32,
        offset_x: 0.0,
        offset_y: 0.0,
    })
}

/// Dispatch the four stroke sizes (left/right horizontal, top/bottom vertical)
/// for a single box-drawing code point in U+2500..U+257F (excluding diagonals).
fn box_line_strokes(
    c: char,
    stroke_size: usize,
    heavy_stroke_size: usize,
) -> (usize, usize, usize, usize) {
    // Left horizontal.
    let h1 = match c {
        '\u{2500}' | '\u{2510}' | '\u{2512}' | '\u{2518}' | '\u{251a}' | '\u{2524}'
        | '\u{2526}' | '\u{2527}' | '\u{2528}' | '\u{252c}' | '\u{252e}' | '\u{2530}'
        | '\u{2532}' | '\u{2534}' | '\u{2536}' | '\u{2538}' | '\u{253a}' | '\u{253c}'
        | '\u{253e}' | '\u{2540}' | '\u{2541}' | '\u{2542}' | '\u{2544}' | '\u{2546}'
        | '\u{254a}' | '\u{2574}' | '\u{257c}' => stroke_size,
        '\u{2501}' | '\u{2511}' | '\u{2513}' | '\u{2519}' | '\u{251b}' | '\u{2525}'
        | '\u{2529}' | '\u{252a}' | '\u{252b}' | '\u{252d}' | '\u{252f}' | '\u{2531}'
        | '\u{2533}' | '\u{2535}' | '\u{2537}' | '\u{2539}' | '\u{253b}' | '\u{253d}'
        | '\u{253f}' | '\u{2543}' | '\u{2545}' | '\u{2547}' | '\u{2548}' | '\u{2549}'
        | '\u{254b}' | '\u{2578}' | '\u{257e}' => heavy_stroke_size,
        _ => 0,
    };
    // Right horizontal.
    let h2 = match c {
        '\u{2500}' | '\u{250c}' | '\u{250e}' | '\u{2514}' | '\u{2516}' | '\u{251c}'
        | '\u{251e}' | '\u{251f}' | '\u{2520}' | '\u{252c}' | '\u{252d}' | '\u{2530}'
        | '\u{2531}' | '\u{2534}' | '\u{2535}' | '\u{2538}' | '\u{2539}' | '\u{253c}'
        | '\u{253d}' | '\u{2540}' | '\u{2541}' | '\u{2542}' | '\u{2543}' | '\u{2545}'
        | '\u{2549}' | '\u{2576}' | '\u{257e}' => stroke_size,
        '\u{2501}' | '\u{250d}' | '\u{250f}' | '\u{2515}' | '\u{2517}' | '\u{251d}'
        | '\u{2521}' | '\u{2522}' | '\u{2523}' | '\u{252e}' | '\u{252f}' | '\u{2532}'
        | '\u{2533}' | '\u{2536}' | '\u{2537}' | '\u{253a}' | '\u{253b}' | '\u{253e}'
        | '\u{253f}' | '\u{2544}' | '\u{2546}' | '\u{2547}' | '\u{2548}' | '\u{254a}'
        | '\u{254b}' | '\u{257a}' | '\u{257c}' => heavy_stroke_size,
        _ => 0,
    };
    // Top vertical.
    let v1 = match c {
        '\u{2502}' | '\u{2514}' | '\u{2515}' | '\u{2518}' | '\u{2519}' | '\u{251c}'
        | '\u{251d}' | '\u{251f}' | '\u{2522}' | '\u{2524}' | '\u{2525}' | '\u{2527}'
        | '\u{252a}' | '\u{2534}' | '\u{2535}' | '\u{2536}' | '\u{2537}' | '\u{253c}'
        | '\u{253d}' | '\u{253e}' | '\u{253f}' | '\u{2541}' | '\u{2545}' | '\u{2546}'
        | '\u{2548}' | '\u{2575}' | '\u{257d}' => stroke_size,
        '\u{2503}' | '\u{2516}' | '\u{2517}' | '\u{251a}' | '\u{251b}' | '\u{251e}'
        | '\u{2520}' | '\u{2521}' | '\u{2523}' | '\u{2526}' | '\u{2528}' | '\u{2529}'
        | '\u{252b}' | '\u{2538}' | '\u{2539}' | '\u{253a}' | '\u{253b}' | '\u{2540}'
        | '\u{2542}' | '\u{2543}' | '\u{2544}' | '\u{2547}' | '\u{2549}' | '\u{254a}'
        | '\u{254b}' | '\u{2579}' | '\u{257f}' => heavy_stroke_size,
        _ => 0,
    };
    // Bottom vertical.
    let v2 = match c {
        '\u{2502}' | '\u{250c}' | '\u{250d}' | '\u{2510}' | '\u{2511}' | '\u{251c}'
        | '\u{251d}' | '\u{251e}' | '\u{2521}' | '\u{2524}' | '\u{2525}' | '\u{2526}'
        | '\u{2529}' | '\u{252c}' | '\u{252d}' | '\u{252e}' | '\u{252f}' | '\u{253c}'
        | '\u{253d}' | '\u{253e}' | '\u{253f}' | '\u{2540}' | '\u{2543}' | '\u{2544}'
        | '\u{2547}' | '\u{2577}' | '\u{257f}' => stroke_size,
        '\u{2503}' | '\u{250e}' | '\u{250f}' | '\u{2512}' | '\u{2513}' | '\u{251f}'
        | '\u{2520}' | '\u{2522}' | '\u{2523}' | '\u{2527}' | '\u{2528}' | '\u{252a}'
        | '\u{252b}' | '\u{2530}' | '\u{2531}' | '\u{2532}' | '\u{2533}' | '\u{2541}'
        | '\u{2542}' | '\u{2545}' | '\u{2546}' | '\u{2548}' | '\u{2549}' | '\u{254a}'
        | '\u{254b}' | '\u{257b}' | '\u{257d}' => heavy_stroke_size,
        _ => 0,
    };
    (h1, h2, v1, v2)
}

/// Dispatch for double-line box components (U+2550..U+256C).
/// Direct port of Alacritty's logic — see `ref/alacritty/...builtin_font.rs`.
fn draw_double_lines(canvas: &mut Canvas, c: char, stroke_size: usize) {
    let width = canvas.width;
    let height = canvas.height;

    let v_lines = match c {
        '\u{2552}' | '\u{2555}' | '\u{2558}' | '\u{255b}' | '\u{255e}' | '\u{2561}'
        | '\u{2564}' | '\u{2567}' | '\u{256a}' => (canvas.x_center(), canvas.x_center()),
        _ => {
            let vb = canvas.v_line_bounds(canvas.x_center(), stroke_size);
            let left = cmp::max(vb.0 as i32 - 1, 0) as f32;
            let right = cmp::min(vb.1 as i32 + 1, width as i32) as f32;
            (left, right)
        }
    };
    let h_lines = match c {
        '\u{2553}' | '\u{2556}' | '\u{2559}' | '\u{255c}' | '\u{255f}' | '\u{2562}'
        | '\u{2565}' | '\u{2568}' | '\u{256b}' => (canvas.y_center(), canvas.y_center()),
        _ => {
            let hb = canvas.h_line_bounds(canvas.y_center(), stroke_size);
            let top = cmp::max(hb.0 as i32 - 1, 0) as f32;
            let bot = cmp::min(hb.1 as i32 + 1, height as i32) as f32;
            (top, bot)
        }
    };

    let v_left = canvas.v_line_bounds(v_lines.0, stroke_size);
    let v_right = canvas.v_line_bounds(v_lines.1, stroke_size);
    let h_top = canvas.h_line_bounds(h_lines.0, stroke_size);
    let h_bot = canvas.h_line_bounds(h_lines.1, stroke_size);

    let w = width as f32;
    let h = height as f32;

    let (top_left_size, bot_left_size) = match c {
        '\u{2550}' | '\u{256b}' => (canvas.x_center(), canvas.x_center()),
        '\u{2555}'..='\u{2557}' => (v_right.1, v_left.1),
        '\u{255b}'..='\u{255d}' => (v_left.1, v_right.1),
        '\u{2561}'..='\u{2563}' | '\u{256a}' | '\u{256c}' => (v_left.1, v_left.1),
        '\u{2564}'..='\u{2568}' => (canvas.x_center(), v_left.1),
        '\u{2569}' => (v_left.1, canvas.x_center()),
        _ => (0.0, 0.0),
    };
    let (top_right_x, bot_right_x, right_size) = match c {
        '\u{2550}' | '\u{2565}' | '\u{256b}' => (canvas.x_center(), canvas.x_center(), w),
        '\u{2552}'..='\u{2554}' | '\u{2568}' => (v_left.0, v_right.0, w),
        '\u{2558}'..='\u{255a}' => (v_right.0, v_left.0, w),
        '\u{255e}'..='\u{2560}' | '\u{256a}' | '\u{256c}' => (v_right.0, v_right.0, w),
        '\u{2564}' | '\u{2566}' => (canvas.x_center(), v_right.0, w),
        '\u{2567}' | '\u{2569}' => (v_right.0, canvas.x_center(), w),
        _ => (0.0, 0.0, 0.0),
    };
    let (left_top_size, right_top_size) = match c {
        '\u{2551}' | '\u{256a}' => (canvas.y_center(), canvas.y_center()),
        '\u{2558}'..='\u{255c}' | '\u{2568}' => (h_bot.1, h_top.1),
        '\u{255d}' => (h_top.1, h_bot.1),
        '\u{255e}'..='\u{2560}' => (canvas.y_center(), h_top.1),
        '\u{2561}'..='\u{2563}' => (h_top.1, canvas.y_center()),
        '\u{2567}' | '\u{2569}' | '\u{256b}' | '\u{256c}' => (h_top.1, h_top.1),
        _ => (0.0, 0.0),
    };
    let (left_bot_y, right_bot_y, bottom_size) = match c {
        '\u{2551}' | '\u{256a}' => (canvas.y_center(), canvas.y_center(), h),
        '\u{2552}'..='\u{2554}' => (h_top.0, h_bot.0, h),
        '\u{2555}'..='\u{2557}' => (h_bot.0, h_top.0, h),
        '\u{255e}'..='\u{2560}' => (canvas.y_center(), h_bot.0, h),
        '\u{2561}'..='\u{2563}' => (h_bot.0, canvas.y_center(), h),
        '\u{2564}'..='\u{2566}' | '\u{256b}' | '\u{256c}' => (h_bot.0, h_bot.0, h),
        _ => (0.0, 0.0, 0.0),
    };

    canvas.draw_h_line(0.0, h_lines.0, top_left_size, stroke_size);
    canvas.draw_h_line(0.0, h_lines.1, bot_left_size, stroke_size);
    canvas.draw_h_line(top_right_x, h_lines.0, right_size, stroke_size);
    canvas.draw_h_line(bot_right_x, h_lines.1, right_size, stroke_size);
    canvas.draw_v_line(v_lines.0, 0.0, left_top_size, stroke_size);
    canvas.draw_v_line(v_lines.1, 0.0, right_top_size, stroke_size);
    canvas.draw_v_line(v_lines.0, left_bot_y, bottom_size, stroke_size);
    canvas.draw_v_line(v_lines.1, right_bot_y, bottom_size, stroke_size);
}

/// Line width. One-eighth of the cell width, floored to at least 1 px.
#[inline]
fn calculate_stroke_size(cell_width: usize) -> usize {
    cmp::max((cell_width as f32 / 8.0).round() as usize, 1)
}

// ================================================================
// Canvas
// ================================================================

/// Pixel canvas with top-left origin and top-down Y axis.
struct Canvas {
    width: usize,
    height: usize,
    buffer: Vec<u8>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        let buffer = vec![0u8; width * height];
        Self {
            width,
            height,
            buffer,
        }
    }

    #[inline]
    fn y_center(&self) -> f32 {
        self.height as f32 / 2.0
    }

    #[inline]
    fn x_center(&self) -> f32 {
        self.width as f32 / 2.0
    }

    fn h_line_bounds(&self, y: f32, stroke_size: usize) -> (f32, f32) {
        let start = cmp::max((y - stroke_size as f32 / 2.0) as i32, 0) as f32;
        let end = cmp::min((y + stroke_size as f32 / 2.0) as i32, self.height as i32) as f32;
        (start, end)
    }

    fn v_line_bounds(&self, x: f32, stroke_size: usize) -> (f32, f32) {
        let start = cmp::max((x - stroke_size as f32 / 2.0) as i32, 0) as f32;
        let end = cmp::min((x + stroke_size as f32 / 2.0) as i32, self.width as i32) as f32;
        (start, end)
    }

    fn draw_h_line(&mut self, x: f32, y: f32, size: f32, stroke_size: usize) {
        let (start, end) = self.h_line_bounds(y, stroke_size);
        self.draw_rect(x, start, size, end - start, COLOR_FILL);
    }

    fn draw_v_line(&mut self, x: f32, y: f32, size: f32, stroke_size: usize) {
        let (start, end) = self.v_line_bounds(x, stroke_size);
        self.draw_rect(start, y, end - start, size, COLOR_FILL);
    }

    fn draw_rect(&mut self, x: f32, y: f32, width: f32, height: f32, color: u8) {
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let start_x = x.max(0.0) as usize;
        let end_x = cmp::min((x + width) as usize, self.width);
        let start_y = y.max(0.0) as usize;
        let end_y = cmp::min((y + height) as usize, self.height);
        for row in start_y..end_y {
            let base = row * self.width;
            self.buffer[base + start_x..base + end_x].fill(color);
        }
    }

    #[inline]
    fn put_pixel(&mut self, x: f32, y: f32, color: u8) {
        if x < 0.0 || y < 0.0 || x > self.width as f32 - 1.0 || y > self.height as f32 - 1.0 {
            return;
        }
        let index = x as usize + y as usize * self.width;
        if color > self.buffer[index] {
            self.buffer[index] = color;
        }
    }

    fn fill(&mut self, color: u8) {
        self.buffer.fill(color);
    }

    /// Draws a quarter-circle centred at `(0, self.height - radius)` with radius
    /// `min(width, height)` plus an attached rectangle to form a '╯'-style curve
    /// in the top-left quadrant. Mirror the buffer afterwards to get the other
    /// three corners.
    fn draw_rounded_corner(&mut self, stroke_size: usize) {
        let radius = (self.width.min(self.height) + stroke_size) as f32 / 2.0;
        let stroke = stroke_size as f32;

        let mut x_offset = 0.0;
        let mut y_offset = 0.0;
        let (long_side, short_side, offset) = if self.height > self.width {
            (&self.height, &self.width, &mut y_offset)
        } else {
            (&self.width, &self.height, &mut x_offset)
        };
        let distance_bias = if short_side % 2 == stroke_size % 2 {
            0.0
        } else {
            0.5
        };
        *offset = *long_side as f32 / 2.0 - radius + stroke / 2.0;
        if (self.width % 2 != self.height % 2) && (long_side % 2 == stroke_size % 2) {
            *offset += 1.0;
        }

        let radius_i = (short_side + stroke_size).div_ceil(2);
        for y in 0..radius_i {
            for x in 0..radius_i {
                let y = y as f32;
                let x = x as f32;
                let distance = x.hypot(y) + distance_bias;
                let value = if distance < radius - stroke - 1.0 {
                    0.0
                } else if distance < radius - stroke {
                    1.0 + distance - (radius - stroke)
                } else if distance < radius - 1.0 {
                    1.0
                } else if distance < radius {
                    radius - distance
                } else {
                    0.0
                };
                self.put_pixel(
                    x + x_offset,
                    y + y_offset,
                    (COLOR_FILL as f32 * value) as u8,
                );
            }
        }

        if self.height > self.width {
            self.draw_rect(
                self.x_center() - stroke * 0.5,
                0.0,
                stroke,
                y_offset,
                COLOR_FILL,
            );
        } else {
            self.draw_rect(
                0.0,
                self.y_center() - stroke * 0.5,
                x_offset,
                stroke,
                COLOR_FILL,
            );
        }
    }

    /// Mirror the canvas along the vertical axis, respecting the extra offset
    /// needed when the stroke thickness and width have different parities.
    fn flip_horizontal_with_stroke(&mut self, stroke_size: usize) {
        let center = (self.width / 2) as usize;
        let extra_offset = usize::from(stroke_size % 2 != self.width % 2);
        let width = self.width;
        for y in 1..=self.height {
            let left = (y - 1) * width;
            let right = y * width - 1;
            if extra_offset != 0 && right >= left {
                self.buffer[right] = self.buffer[left];
            }
            for offset in 0..center {
                let a = left + offset;
                let b = right - offset - extra_offset;
                if a < self.buffer.len() && b < self.buffer.len() && a != b {
                    self.buffer.swap(a, b);
                }
            }
        }
    }

    /// Mirror the canvas along the horizontal axis.
    fn flip_vertical_with_stroke(&mut self, stroke_size: usize) {
        let center = (self.height / 2) as usize;
        let extra_offset = usize::from(stroke_size % 2 != self.height % 2);
        let width = self.width;
        if extra_offset != 0 && self.height > 0 {
            let bottom_row = (self.height - 1) * width;
            for i in 0..width {
                self.buffer[bottom_row + i] = self.buffer[i];
            }
        }
        for offset in 1..=center {
            let top_row = (offset - 1) * width;
            let bottom_row = self.height.saturating_sub(offset + extra_offset) * width;
            if top_row >= self.buffer.len() || bottom_row >= self.buffer.len() {
                continue;
            }
            for i in 0..width {
                self.buffer.swap(top_row + i, bottom_row + i);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_supported_chars_render() {
        for c in ('\u{2500}'..='\u{2570}').chain('\u{2574}'..='\u{259f}') {
            let g = builtin_glyph(c, 8, 16);
            assert!(g.is_some(), "missing glyph for U+{:04X}", c as u32);
            let g = g.unwrap();
            assert_eq!(g.width, 8);
            assert_eq!(g.height, 16);
            assert_eq!(g.data.len(), 8 * 16);
        }
    }

    #[test]
    fn unsupported_chars_rejected() {
        assert!(builtin_glyph('A', 8, 16).is_none());
        assert!(builtin_glyph('\u{2571}', 8, 16).is_none()); // diagonal
    }

    #[test]
    fn full_block_fully_covered() {
        let g = builtin_glyph('\u{2588}', 8, 16).unwrap();
        assert!(g.data.iter().all(|&p| p == COLOR_FILL));
    }

    #[test]
    fn horizontal_line_spans_width() {
        let g = builtin_glyph('\u{2500}', 8, 16).unwrap();
        // The 1-pixel line lands somewhere around y_center. Check that ALL
        // 8 columns are filled in at least one row, and that the line spans
        // the full width with no gaps.
        let mut found = false;
        for y in 0..16usize {
            let row_filled: usize = (0..8).filter(|&x| g.data[y * 8 + x] > 0).count();
            if row_filled == 8 {
                found = true;
                break;
            }
        }
        assert!(found, "no row had a continuous full-width line");
    }

    #[test]
    fn vertical_line_spans_height() {
        let g = builtin_glyph('\u{2502}', 8, 16).unwrap();
        // Find the column that has the most opaque pixels and check it spans
        // the full height — a vertical line should cover all 16 rows.
        let mut max_filled = 0;
        for x in 0..8usize {
            let col_filled: usize = (0..16).filter(|&y| g.data[y * 8 + x] > 0).count();
            max_filled = max_filled.max(col_filled);
        }
        assert_eq!(max_filled, 16, "vertical line did not span full height");
    }
}
