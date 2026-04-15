//! # glow - Terminal image display
//!
//! Supports kitty graphics protocol, sixel, and w3m image display.
//! Feature clone of termpix (Ruby).
//!
//! ```no_run
//! use glow::Display;
//! let mut display = Display::new();
//! if display.supported() {
//!     display.show("image.png", 1, 1, 80, 24);
//! }
//! ```

use base64::Engine;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Protocol {
    Kitty,
    Sixel,
    W3m,
    Chafa,
}

/// Pre-converted PNG data cache, shareable across threads.
pub type PngCache = std::sync::Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>>;

pub fn new_png_cache() -> PngCache {
    std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()))
}

/// Pre-convert images to PNG in background. Call from a spawned thread.
pub fn preconvert_images(paths: &[String], pixel_width: u32, cache: &PngCache) {
    for path_str in paths {
        let mtime = std::fs::metadata(path_str)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0);
        let key = format!("{}:{}:{}", path_str, pixel_width, mtime);

        // Skip if already cached
        if let Ok(c) = cache.lock() {
            if c.contains_key(&key) { continue; }
        }

        let output = Command::new("convert")
            .arg(format!("{}[0]", path_str))
            .arg("-auto-orient")
            .arg("-resize")
            .arg(format!("{}>", pixel_width))
            .arg("PNG:-")
            .output();

        if let Ok(o) = output {
            if !o.stdout.is_empty() {
                if let Ok(mut c) = cache.lock() {
                    if c.len() > 32 { c.clear(); } // keep cache bounded
                    c.insert(key, o.stdout);
                }
            }
        }
    }
}

pub struct Display {
    protocol: Option<Protocol>,
    active_ids: Vec<u32>,
    image_cache: HashMap<String, (u32, u16)>,  // (image_id, natural_rows)
    pub png_cache: PngCache,
}

impl Display {
    /// Auto-detect the best protocol
    pub fn new() -> Self {
        let protocol = detect_protocol();
        Self {
            protocol,
            active_ids: Vec::new(),
            image_cache: HashMap::new(),
            png_cache: new_png_cache(),
        }
    }

    /// Force a specific display mode ("auto", "ascii", "off", "kitty", "sixel")
    pub fn with_mode(mode: &str) -> Self {
        let protocol = match mode {
            "ascii" | "chafa" => {
                if command_exists("chafa") { Some(Protocol::Chafa) } else { None }
            }
            "kitty" => Some(Protocol::Kitty),
            "sixel" => Some(Protocol::Sixel),
            "off" | "none" => None,
            _ => detect_protocol(), // "auto"
        };
        Self {
            protocol,
            active_ids: Vec::new(),
            image_cache: HashMap::new(),
            png_cache: new_png_cache(),
        }
    }

    /// Check if image display is supported
    pub fn supported(&self) -> bool {
        self.protocol.is_some()
    }

    /// Get the detected protocol
    pub fn protocol(&self) -> Option<Protocol> {
        self.protocol
    }

    /// Display an image at the specified character position
    pub fn show(&mut self, image_path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
        let proto = match self.protocol {
            Some(p) => p,
            None => return false,
        };
        if !Path::new(image_path).exists() {
            return false;
        }

        match proto {
            Protocol::Kitty => self.kitty_display(image_path, x, y, max_width, max_height),
            Protocol::Sixel => sixel_display(image_path, x, y, max_width, max_height),
            Protocol::W3m => w3m_display(image_path, x, y, max_width, max_height),
            Protocol::Chafa => chafa_display(image_path, x, y, max_width, max_height),
        }
    }

    /// Clear all displayed images
    pub fn clear(&mut self, x: u16, y: u16, width: u16, height: u16, term_width: u16, term_height: u16) {
        match self.protocol {
            Some(Protocol::Kitty) => {
                for id in &self.active_ids {
                    print!("\x1b_Ga=d,d=i,i={},q=2\x1b\\", id);
                }
                if !self.active_ids.is_empty() {
                    io::stdout().flush().ok();
                }
                self.active_ids.clear();
            }
            Some(Protocol::Sixel) => {
                // Sixel images are inline, cleared by terminal redraw
            }
            Some(Protocol::W3m) => {
                w3m_clear(x, y, width, height, term_width, term_height);
            }
            Some(Protocol::Chafa) => {
                // Chafa is text-based, cleared by terminal redraw
            }
            None => {}
        }
    }

    // --- Kitty protocol ---

    fn kitty_display(&mut self, image_path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
        let (cell_w, cell_h) = get_cell_size();
        if cell_w == 0 || cell_h == 0 {
            return false;
        }

        let pixel_w = max_width as u32 * cell_w as u32;

        // Cache by path + width + mtime (NOT height, so shrinking reuses cached data)
        let mtime = std::fs::metadata(image_path)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0);
        let cache_key = format!("{}:{}:{}", image_path, pixel_w, mtime);

        let (image_id, natural_rows) = if let Some(&cached) = self.image_cache.get(&cache_key) {
            cached
        } else {
            // Generate new ID
            let id = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() % 4294967295) as u32;
            let id = if id == 0 { 1 } else { id };

            // Check pre-convert cache first (populated by background thread)
            let png_data = if let Ok(mut c) = self.png_cache.lock() {
                c.remove(&cache_key)
            } else { None };

            let png_data = match png_data {
                Some(data) => data,
                None => {
                    // Fallback: convert synchronously
                    let output = Command::new("convert")
                        .arg(format!("{}[0]", image_path))
                        .arg("-auto-orient")
                        .arg("-resize")
                        .arg(format!("{}>", pixel_w))
                        .arg("PNG:-")
                        .output();
                    match output {
                        Ok(o) if !o.stdout.is_empty() => o.stdout,
                        _ => return false,
                    }
                }
            };

            // Get actual image height from PNG header to compute natural row count
            let img_pixel_h = png_height(&png_data).unwrap_or(pixel_w);
            let nat_rows = ((img_pixel_h as f64) / (cell_h as f64)).ceil() as u16;

            // Encode and transmit in chunks
            let encoded = base64::engine::general_purpose::STANDARD.encode(&png_data);
            let chunks: Vec<&str> = encoded.as_bytes()
                .chunks(4096)
                .map(|c| std::str::from_utf8(c).unwrap_or(""))
                .collect();

            for (idx, chunk) in chunks.iter().enumerate() {
                let more = if idx < chunks.len() - 1 { 1 } else { 0 };
                if idx == 0 {
                    print!("\x1b_Ga=t,f=100,i={},q=2,m={};{}\x1b\\", id, more, chunk);
                } else {
                    print!("\x1b_Gm={};{}\x1b\\", more, chunk);
                }
            }
            io::stdout().flush().ok();
            self.image_cache.insert(cache_key, (id, nat_rows));
            (id, nat_rows)
        };

        // Delete previous placement, then place at new position.
        print!("\x1b_Ga=d,d=i,i={},q=2\x1b\\", image_id);
        print!("\x1b[{};{}H", y, x);
        if max_height < natural_rows && natural_rows > 0 {
            let scale_cols = (max_width as u32 * max_height as u32 / natural_rows as u32).max(1) as u16;
            print!("\x1b_Ga=p,i={},c={},r={},q=2,C=1\x1b\\", image_id, scale_cols, max_height);
        } else {
            print!("\x1b_Ga=p,i={},q=2,C=1\x1b\\", image_id);
        }
        io::stdout().flush().ok();

        if !self.active_ids.contains(&image_id) {
            self.active_ids.push(image_id);
        }
        true
    }
}

// --- Sixel protocol ---

fn sixel_display(image_path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
    let pixel_w = max_width as u32 * 10;
    let pixel_h = max_height as u32 * 20;
    print!("\x1b[{};{}H", y, x);
    io::stdout().flush().ok();
    let escaped = shell_escape(image_path);
    Command::new("convert")
        .arg(&escaped)
        .arg("-resize")
        .arg(format!("{}x{}\\>", pixel_w, pixel_h))
        .arg("sixel:-")
        .status()
        .is_ok()
}

// --- W3m protocol ---

fn w3m_display(image_path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
    let (term_w, term_h, cols, rows) = get_terminal_pixel_size();
    if term_w == 0 || cols == 0 {
        return false;
    }
    let char_w = term_w / cols;
    let char_h = term_h / rows;

    let img_x = char_w * x as u32;
    let img_y = char_h * y as u32;
    let img_max_w = char_w * max_width as u32;
    let img_max_h = char_h * max_height as u32;

    // Get image dimensions
    let escaped = shell_escape(image_path);
    let dims = Command::new("identify")
        .arg("-format")
        .arg("%wx%h")
        .arg(format!("{}[0]", escaped))
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let dims = dims.trim();
    let (mut img_w, mut img_h) = match dims.split_once('x') {
        Some((w, h)) => (w.parse::<u32>().unwrap_or(0), h.parse::<u32>().unwrap_or(0)),
        None => return false,
    };
    if img_w == 0 || img_h == 0 {
        return false;
    }

    // Scale to fit
    if img_w > img_max_w || img_h > img_max_h {
        let scale = (img_max_w as f64 / img_w as f64).min(img_max_h as f64 / img_h as f64);
        img_w = (img_w as f64 * scale) as u32;
        img_h = (img_h as f64 * scale) as u32;
    }

    let cmd = format!("0;1;{};{};{};{};;;;;{}\n4;\n3;\n", img_x, img_y, img_w, img_h, image_path);
    let mut child = match Command::new("/usr/lib/w3m/w3mimgdisplay")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(ref mut stdin) = child.stdin {
        let _ = stdin.write_all(cmd.as_bytes());
    }
    let _ = child.wait();
    true
}

fn w3m_clear(x: u16, y: u16, width: u16, height: u16, term_width: u16, term_height: u16) {
    let (term_w, term_h, _, _) = get_terminal_pixel_size();
    if term_w == 0 {
        return;
    }
    let char_w = term_w / term_width as u32;
    let char_h = term_h / term_height as u32;

    let img_x = (char_w * x as u32).saturating_sub(char_w);
    let img_y = char_h * y as u32;
    let img_max_w = char_w * width as u32 + char_w + 2;
    let img_max_h = char_h * height as u32 + 2;

    let cmd = format!("6;{};{};{};{};\n4;\n3;\n", img_x, img_y, img_max_w, img_max_h);
    if let Ok(mut child) = Command::new("/usr/lib/w3m/w3mimgdisplay")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        if let Some(ref mut stdin) = child.stdin {
            let _ = stdin.write_all(cmd.as_bytes());
        }
        let _ = child.wait();
    }
}

// --- Chafa ASCII art ---

fn chafa_display(image_path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
    let output = Command::new("chafa")
        .args([
            "--size", &format!("{}x{}", max_width, max_height),
            "--animate", "off",
            "--format", "symbols",  // Force text symbols, not sixel/kitty
            "--color-space", "din99d",
        ])
        .arg(image_path)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            for (i, line) in text.lines().enumerate() {
                if i >= max_height as usize { break; }
                print!("\x1b[{};{}H{}", y + i as u16, x, line);
            }
            io::stdout().flush().ok();
            true
        }
        _ => false,
    }
}

// --- Protocol detection ---

/// Drain kitty graphics protocol responses from stdin.
/// Kitty sends back "\x1b_Gi=ID;OK\x1b\\" after receiving image data.
/// These must be consumed to prevent them from leaking into the input stream.
fn drain_kitty_responses() {
    use std::io::Read;
    // Small delay to let the terminal send its response
    std::thread::sleep(std::time::Duration::from_millis(10));
    // Read and discard any pending stdin data (non-blocking)
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let mut buf = [0u8; 1024];
        unsafe {
            // Get current flags
            let flags = libc::fcntl(fd, libc::F_GETFL);
            // Set non-blocking
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            // Read and discard
            while libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) > 0 {}
            // Restore blocking mode
            libc::fcntl(fd, libc::F_SETFL, flags);
        }
    }
}

fn detect_protocol() -> Option<Protocol> {
    // Kitty
    if std::env::var("TERM").unwrap_or_default() == "xterm-kitty"
        || std::env::var("KITTY_WINDOW_ID").is_ok()
        || std::env::var("TERM_PROGRAM").unwrap_or_default() == "WezTerm"
        || std::env::var("WEZTERM_EXECUTABLE").is_ok()
    {
        if command_exists("convert") {
            return Some(Protocol::Kitty);
        }
    }

    // Sixel (xterm, mlterm, foot)
    let term = std::env::var("TERM").unwrap_or_default();
    if term.starts_with("xterm") && term != "xterm-kitty" || term.starts_with("mlterm") || term.starts_with("foot") {
        if command_exists("convert") {
            return Some(Protocol::Sixel);
        }
    }

    // W3m fallback
    if Path::new("/usr/lib/w3m/w3mimgdisplay").exists() {
        if command_exists("xwininfo") && command_exists("xdotool") && command_exists("identify") {
            return Some(Protocol::W3m);
        }
    }

    // Chafa ASCII art fallback (works in any terminal)
    if command_exists("chafa") {
        return Some(Protocol::Chafa);
    }

    None
}

fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Extract height from PNG IHDR chunk (bytes 20-23, big-endian u32)
fn png_height(data: &[u8]) -> Option<u32> {
    if data.len() >= 24 && &data[0..4] == b"\x89PNG" {
        Some(u32::from_be_bytes([data[20], data[21], data[22], data[23]]))
    } else {
        None
    }
}

pub fn get_cell_size() -> (u16, u16) {
    // Try to get pixel size from terminal
    if let Ok((rows, cols)) = crossterm_size() {
        // Try ioctl for pixel dimensions
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) };
        if result == 0 && ws.ws_xpixel > 0 && ws.ws_ypixel > 0 {
            return (ws.ws_xpixel / cols, ws.ws_ypixel / rows);
        }
    }
    // Default: 10x20
    (10, 20)
}

fn crossterm_size() -> Result<(u16, u16), ()> {
    // rows, cols via ioctl
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) };
    if result == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        Ok((ws.ws_row, ws.ws_col))
    } else {
        Err(())
    }
}

fn get_terminal_pixel_size() -> (u32, u32, u32, u32) {
    let output = Command::new("sh")
        .arg("-c")
        .arg("xwininfo -id $(xdotool getactivewindow 2>/dev/null) 2>/dev/null")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();

    let w = output.lines().find_map(|l| {
        l.trim().strip_prefix("Width: ").and_then(|v| v.parse::<u32>().ok())
    }).unwrap_or(0);
    let h = output.lines().find_map(|l| {
        l.trim().strip_prefix("Height: ").and_then(|v| v.parse::<u32>().ok())
    }).unwrap_or(0);

    let cols = Command::new("tput").arg("cols").output()
        .ok().and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(80);
    let rows = Command::new("tput").arg("lines").output()
        .ok().and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(24);

    (w, h, cols, rows)
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
