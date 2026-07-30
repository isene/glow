//! Three renderings of one image, side by side: the real pixels through
//! the kitty graphics protocol, glow's own half blocks, and chafa.
//!
//! ```sh
//! imgcompare photo.jpg
//! ```
//!
//! Each panel is timed. Needs a wide window. Any key puts the screen
//! back.

use glow::{Display, Protocol};
use std::io::{Read, Write};
use std::time::Instant;

const PANELS: [(&str, &str); 3] = [
    ("kitty", "the real pixels"),
    ("halfblock", "glow, two colours per cell"),
    ("chafa", "chafa, for comparison"),
];

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: imgcompare IMAGE");
        std::process::exit(1);
    };
    if !std::path::Path::new(&path).is_file() {
        eprintln!("imgcompare: no such file: {path}");
        std::process::exit(1);
    }

    let (cols, rows) = glow::terminal_size();
    let gap = 2u16;
    let pw = (cols.saturating_sub(gap * 2)) / 3;
    let ph = rows.saturating_sub(4);
    if pw < 16 || ph < 6 {
        eprintln!("imgcompare: needs a window at least 52 columns by 10 rows");
        std::process::exit(1);
    }

    // Does this terminal actually speak the kitty protocol? Sending the
    // escapes to one that does not paints garbage over the other panels.
    let kitty = matches!(Display::new().protocol(), Some(Protocol::Kitty));

    let saved = raw_mode();
    print!("\x1b[2J\x1b[H\x1b[?25l\x1b[1m{path}\x1b[0m");

    let mut shown: Vec<Display> = Vec::new();
    for (i, (mode, label)) in PANELS.iter().enumerate() {
        let x = 1 + i as u16 * (pw + gap);
        print!("\x1b[2;{x}H\x1b[1m{mode}\x1b[0m \x1b[2m{label}\x1b[0m");
        std::io::stdout().flush().ok();
        if *mode == "kitty" && !kitty {
            note(x, "this terminal has no kitty protocol");
            continue;
        }
        let mut d = Display::with_mode(mode);
        if d.protocol().is_none() {
            note(x, &format!("{mode} is not available here"));
            continue;
        }
        let mut ok = true;
        let ms = painted(|| ok = d.show(&path, x, 3, pw, ph));
        if ok {
            print!("\x1b[2;{}H\x1b[2m{ms} ms\x1b[0m", x + pw.saturating_sub(6));
            std::io::stdout().flush().ok();
        } else {
            note(x, &format!("{mode} rendered nothing"));
        }
        shown.push(d);
    }

    print!("\x1b[{rows};1H\x1b[2many key to finish\x1b[0m");
    std::io::stdout().flush().ok();
    let mut byte = [0u8; 1];
    std::io::stdin().read_exact(&mut byte).ok();

    // Kitty placements outlive the process unless someone deletes them.
    for d in shown.iter_mut() {
        d.clear_all();
    }
    print!("\x1b[?25h\x1b[2J\x1b[H");
    std::io::stdout().flush().ok();
    restore(saved);
}

fn note(x: u16, msg: &str) {
    print!("\x1b[4;{x}H\x1b[2m{msg}\x1b[0m");
    std::io::stdout().flush().ok();
}

/// How long the panel took: decode, scale, and get the bytes out.
///
/// This covers more than it looks. A quarter of a megabyte of escape
/// sequences does not fit in a pty buffer, so the write blocks until the
/// terminal has chewed through most of it. What it cannot see is the
/// last screenful, and any repaint the terminal does after we are done.
fn painted(f: impl FnOnce()) -> u128 {
    let start = Instant::now();
    f();
    std::io::stdout().flush().ok();
    start.elapsed().as_millis()
}

// --- raw mode, so the cursor-position reply does not echo ---

fn raw_mode() -> Option<libc::termios> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return None;
        }
        let saved = t;
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(0, libc::TCSANOW, &t);
        Some(saved)
    }
}

fn restore(saved: Option<libc::termios>) {
    if let Some(t) = saved {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &t);
        }
    }
}
