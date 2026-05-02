//! Tiny standalone bench that mirrors vtebench's `scrolling_*_small_region`
//! payload but feeds the bytes straight into the VTE parser, with no PTY,
//! no GPU and no event loop in the way.
//!
//! Run with:
//!     cargo run --release --example bench_scrolling -p nexterm-vte
//!
//! If the parser itself is fast for both layouts, the regression seen in
//! vtebench lives downstream (rendering / event loop / vsync). If parser
//! throughput already differs by orders of magnitude, the bug is in here.

use std::time::Instant;

use nexterm_vte::parser::TerminalParser;

const COLS: usize = 80;
const ROWS: usize = 24;
const PAYLOAD_BYTES: usize = 1 << 20; // 1 MiB, matches vtebench default

fn build_payload() -> Vec<u8> {
    // vtebench's `scrolling/benchmark` is a single `printf "y\n"`. The host
    // PTY's ONLCR maps "\n" to "\r\n" before it reaches the terminal, so the
    // actual byte stream is "y\r\n". vtebench then repeats the script's
    // output until min_bytes (1 MiB) is reached.
    let unit: &[u8] = b"y\r\n";
    let n = (PAYLOAD_BYTES + unit.len() - 1) / unit.len();
    let mut buf = Vec::with_capacity(n * unit.len());
    for _ in 0..n {
        buf.extend_from_slice(unit);
    }
    buf
}

/// Run `setup` (alt-screen + DECSTBM) followed by `payload` and return the
/// wall time the parser spent consuming the payload.
fn run(name: &str, setup: &[u8], payload: &[u8]) {
    let mut parser = TerminalParser::new(COLS, ROWS);

    // Setup is excluded from the timer to mirror vtebench.
    parser.process(setup);

    let start = Instant::now();
    parser.process(payload);
    let elapsed = start.elapsed();

    let mb = payload.len() as f64 / (1024.0 * 1024.0);
    let throughput = mb / elapsed.as_secs_f64();
    println!(
        "  {name:<32}  {bytes:>9} bytes  {ms:>8.2} ms  {th:>8.2} MiB/s",
        bytes = payload.len(),
        ms = elapsed.as_secs_f64() * 1000.0,
        th = throughput,
    );
}

fn main() {
    let payload = build_payload();

    // vtebench setups (decoded from the upstream shell scripts):
    //   scrolling_top_small_region/setup:
    //     printf "\e[?1049h\e[1;$((lines / 2))r"
    //   scrolling_bottom_small_region/setup:
    //     printf "\e[?1049h\e[$((lines / 2));${lines}r"
    // With lines=24, that becomes:
    let half = ROWS / 2;
    let setup_top = format!("\x1b[?1049h\x1b[1;{}r", half).into_bytes();
    let setup_bot = format!("\x1b[?1049h\x1b[{};{}r", half, ROWS).into_bytes();
    let setup_full = b"\x1b[?1049h".to_vec();
    let setup_none = Vec::new();

    println!("nexterm-vte parser micro-bench (1 MiB of 'y\\r\\n')");
    println!();
    run("primary screen, no region", &setup_none, &payload);
    run("alt screen, no region", &setup_full, &payload);
    run("alt + top half region", &setup_top, &payload);
    run("alt + bottom half region", &setup_bot, &payload);

    // Repeat each twice so we can spot warm-up effects.
    println!();
    println!("(second pass)");
    run("primary screen, no region", &setup_none, &payload);
    run("alt screen, no region", &setup_full, &payload);
    run("alt + top half region", &setup_top, &payload);
    run("alt + bottom half region", &setup_bot, &payload);
}
