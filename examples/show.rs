//! Show an image in the terminal with one of glow's renderers.
//!
//! ```sh
//! cargo run --release --example show -- halfblock picture.jpg 80 24
//! ```
//!
//! Modes: auto, halfblock, braille, chafa, ascii, kitty, sixel.

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 2 {
        eprintln!("usage: show MODE IMAGE [COLS] [ROWS]");
        std::process::exit(1);
    }
    let cols: u16 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(80);
    let rows: u16 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(24);
    let mut d = glow::Display::with_mode(&a[0]);
    print!("\x1b[2J\x1b[H");
    if !d.show(&a[1], 1, 1, cols, rows) {
        eprintln!("glow: nothing rendered ({} mode)", a[0]);
    }
    print!("\x1b[{};1H", rows + 2);
}
