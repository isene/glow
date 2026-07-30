//! Three renderings of one image, side by side: the real pixels through
//! the kitty graphics protocol, glow's own half blocks, and chafa.
//!
//! ```sh
//! imgcompare photo.jpg
//! ```
//!
//! Needs a wide window. Press Enter to put the screen back.

use glow::{Display, Protocol};
use std::io::{BufRead, Write};

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

    print!("\x1b[2J\x1b[H\x1b[?25l");
    println!("\x1b[1m{path}\x1b[0m");

    let mut shown: Vec<Display> = Vec::new();
    for (i, (mode, label)) in PANELS.iter().enumerate() {
        let x = 1 + i as u16 * (pw + gap);
        print!("\x1b[2;{x}H\x1b[1m{mode}\x1b[0m \x1b[2m{label}\x1b[0m");
        // Flush the label before rendering: chafa can take a second on a
        // big picture, and a buffered label would appear only after it.
        std::io::stdout().flush().ok();
        if *mode == "kitty" && !kitty {
            print!("\x1b[4;{x}H\x1b[2mthis terminal has no kitty protocol\x1b[0m");
            continue;
        }
        let mut d = Display::with_mode(mode);
        if d.protocol().is_none() {
            print!("\x1b[4;{x}H\x1b[2m{mode} is not available here\x1b[0m");
            continue;
        }
        if !d.show(&path, x, 3, pw, ph) {
            print!("\x1b[4;{x}H\x1b[2m{mode} rendered nothing\x1b[0m");
        }
        shown.push(d);
    }

    print!("\x1b[{};1H\x1b[2mEnter to finish\x1b[0m", rows);
    std::io::stdout().flush().ok();
    std::io::stdin().lock().read_line(&mut String::new()).ok();

    // Kitty placements outlive the process unless someone deletes them.
    for d in shown.iter_mut() {
        d.clear_all();
    }
    print!("\x1b[?25h\x1b[2J\x1b[H");
    std::io::stdout().flush().ok();
}
