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

pub mod fb;

use base64::Engine;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Protocol {
    Kitty,
    /// The pixels of a bare console, written straight to `/dev/fb0`.
    /// No terminal is involved: the picture goes on the screen itself,
    /// and the console keeps drawing its text over the top.
    Framebuffer,
    Sixel,
    W3m,
    Chafa,
    /// Two pixels per cell: `▀` with the top half in the foreground
    /// colour and the bottom in the background. Needs truecolour and
    /// nothing else — no external program, no graphics protocol. The
    /// best-looking fallback for photographs, and the default one.
    HalfBlock,
    /// Universal text fallback: render the image into Unicode braille
    /// glyphs (`U+2800`–`U+28FF`). Each cell holds a 2×4 dot grid, so a
    /// W×H char block yields a (2W)×(4H) "pixel" image. No external
    /// dependency beyond `convert`. Works over SSH, in tmux without
    /// passthrough, and on every terminal that can render Unicode.
    Braille,
    /// Plain-ASCII fallback: one `" .:-=+*#%@"`-ramp glyph per cell.
    /// The lowest common denominator — the Linux virtual console
    /// (`TERM=linux`) has no braille block (`U+2800`) in its font, so
    /// Braille renders blank there; ASCII always shows. `convert` only.
    Ascii,
}

/// Pre-converted PNG data cache, shareable across threads.
pub type PngCache = std::sync::Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>>;

pub fn new_png_cache() -> PngCache {
    std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()))
}

/// FNV-1a 64-bit hash. Used to turn a cache key (which is a path plus
/// sizes plus mtime, e.g. `/home/u/x.jpg:800x600:1700000000`) into a
/// filesystem-safe disk-cache filename.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Per-user on-disk PNG cache directory. Lives under
/// `~/.kastrup/image_cache/` so it shares space with kastrup's own
/// attachment cache. Phase 1 of the glow image speedup plan: cache
/// PERSISTS across process restarts (the in-RAM `PngCache` is
/// per-process and gets wiped on every kastrup launch).
fn disk_cache_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".kastrup").join("image_cache"))
}

fn disk_cache_path(key: &str) -> Option<std::path::PathBuf> {
    let dir = disk_cache_dir()?;
    Some(dir.join(format!("{:016x}.png", fnv1a64(key))))
}

fn disk_cache_read(key: &str) -> Option<Vec<u8>> {
    std::fs::read(disk_cache_path(key)?).ok()
}

fn disk_cache_write(key: &str, data: &[u8]) {
    let Some(path) = disk_cache_path(key) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, data);
}

/// Two-tier cache get: RAM first, fall back to disk. On disk hit,
/// populate RAM so the next lookup is fast. Returns owned `Vec<u8>`.
fn cache_get(cache: &PngCache, key: &str) -> Option<Vec<u8>> {
    if let Ok(c) = cache.lock() {
        if let Some(v) = c.get(key) { return Some(v.clone()); }
    }
    let v = disk_cache_read(key)?;
    if let Ok(mut c) = cache.lock() {
        c.insert(key.to_string(), v.clone());
    }
    Some(v)
}

/// Two-tier cache contains: RAM-or-disk check used by the
/// preconvert path to decide whether `convert` needs to run at all.
fn cache_contains(cache: &PngCache, key: &str) -> bool {
    if let Ok(c) = cache.lock() {
        if c.contains_key(key) { return true; }
    }
    disk_cache_path(key).map(|p| p.exists()).unwrap_or(false)
}

/// Two-tier cache put: write to disk first (so a crash before the
/// next get still leaves the bytes available), then insert into RAM.
/// Caps RAM at 256 entries — disk has no soft cap (the user's image
/// cache directory is theirs to prune).
fn cache_put(cache: &PngCache, key: String, data: Vec<u8>) {
    disk_cache_write(&key, &data);
    if let Ok(mut c) = cache.lock() {
        if c.len() >= 256 {
            let to_drop: Vec<String> = c.keys().take(c.len().saturating_sub(200))
                .cloned().collect();
            for k in to_drop { c.remove(&k); }
        }
        c.insert(key, data);
    }
}

/// Phase 2 — Rust-side resize + transparent extent.
///
/// Reads the image file, downscales to fit within `(max_w, max_h)`
/// (preserving aspect ratio, never enlarging — same semantics as
/// ImageMagick's `WxH>` modifier), pads up to the next `(cell_w,
/// cell_h)` multiple at NorthWest gravity so kitty can place it at
/// integer cell dimensions without stretching, and re-encodes as
/// PNG. Returns the PNG bytes.
///
/// Returns None on unsupported formats / decode errors — the caller
/// drops to the `magick` subprocess fallback which handles HEIC /
/// SVG / weird CMYK JPEGs etc. that the `image` crate doesn't cover.
fn rust_resize_and_pad(
    path: &str,
    max_w: u32, max_h: u32,
    cell_w: u32, cell_h: u32,
) -> Option<Vec<u8>> {
    use image::{ImageReader, ImageBuffer, Rgba, imageops::FilterType};

    let reader = ImageReader::open(path).ok()?
        .with_guessed_format().ok()?;
    let img = reader.decode().ok()?;

    // Resize only if image overflows the bounds.
    let (sw, sh) = (img.width(), img.height());
    let resized = if sw <= max_w && sh <= max_h {
        img
    } else {
        // Triangle filter ≈ bilinear: decent quality, much faster than
        // Lanczos3 on the hot path. Visually indistinguishable at the
        // sub-300px sizes the inline-preview pane uses.
        img.resize(max_w, max_h, FilterType::Triangle)
    };
    let (rw, rh) = (resized.width(), resized.height());

    // Cell-aligned padding (mirrors the magick `-extent` step in the
    // old path). Round each dim up to the next cell multiple so kitty
    // can specify integer `c=cols,r=rows` without stretching content.
    let cw = cell_w.max(1);
    let ch = cell_h.max(1);
    let pad_w = ((rw + cw - 1) / cw) * cw;
    let pad_h = ((rh + ch - 1) / ch) * ch;

    // Skip the compose pass entirely when the resized image already
    // sits on cell boundaries — happens for screenshots, pixel art,
    // any image whose natural dims happen to be cell-aligned.
    if rw == pad_w && rh == pad_h {
        let mut out: Vec<u8> = Vec::new();
        encode_png(&resized.to_rgba8(), rw, rh, &mut out)?;
        return Some(out);
    }

    let mut canvas: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(pad_w, pad_h, Rgba([0, 0, 0, 0]));
    let resized_rgba = resized.to_rgba8();
    image::imageops::overlay(&mut canvas, &resized_rgba, 0, 0);

    let mut out: Vec<u8> = Vec::new();
    encode_png(&canvas, pad_w, pad_h, &mut out)?;
    Some(out)
}

fn encode_png(
    buf: &image::ImageBuffer<image::Rgba<u8>, Vec<u8>>,
    w: u32, h: u32, out: &mut Vec<u8>,
) -> Option<()> {
    use image::codecs::png::{PngEncoder, CompressionType, FilterType as PngFilter};
    use image::ImageEncoder;
    // Adaptive filtering, not none. Measured on a 1024x1312 page: no
    // filter took 10 ms and wrote 2,225 KB; adaptive took 4 ms and wrote
    // 236 KB. Filtering is cheaper than writing the bytes it saves, and
    // every one of those bytes is base64'd, pushed through the pty and
    // inflated by the terminal.
    out.reserve(w as usize * h as usize / 8);
    PngEncoder::new_with_quality(out, CompressionType::Fast, PngFilter::Adaptive)
        .write_image(buf.as_raw(), w, h, image::ExtendedColorType::Rgba8).ok()
}

/// Pre-convert images to PNG in background. Call from a spawned thread.
/// Background-convert a list of images, store cell-aligned (padded)
/// PNG output in `cache`. The padding step runs here so the
/// foreground show path can use the cached entry directly — no
/// second `convert` subprocess at display time.
///
/// `cell_w` / `cell_h` are the host terminal's cell dimensions in
/// pixels. They're passed in (instead of probed inside) because the
/// caller knows them at trigger time and ioctl probing from a worker
/// thread isn't guaranteed to see the right TTY.
///
/// `cancel` is checked between paths so a precache run can bail out
/// quickly when the user navigates away from the directory.
pub fn preconvert_images(
    paths: &[String],
    pixel_width: u32,
    pixel_height: u32,
    cell_w: u16,
    cell_h: u16,
    cache: &PngCache,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    for path_str in paths {
        if let Some(c) = cancel { if c.load(Ordering::Relaxed) { return; } }

        let mtime = std::fs::metadata(path_str)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0);
        let key = format!("{}:{}x{}:{}", path_str, pixel_width, pixel_height, mtime);

        // Skip if already cached (padded data — see insert below).
        // Two-tier: in-RAM HashMap first, then on-disk PNG cache.
        // After a kastrup restart, RAM is empty but disk usually has
        // every recently-viewed image already converted.
        if cache_contains(cache, &key) { continue; }

        // Phase 2: try the Rust `image` crate first. Decode + resize +
        // cell-aligned pad in one pass, no subprocess. Falls through
        // to the magick subprocess path only on decode failure
        // (unsupported format, EXIF orientation we can't trivially
        // handle, etc.).
        if let Some(final_data) = rust_resize_and_pad(
            path_str, pixel_width, pixel_height,
            cell_w as u32, cell_h as u32,
        ) {
            cache_put(cache, key, final_data);
            continue;
        }

        // Resize.
        let output = Command::new(imagemagick_cmd())
            .arg(format!("{}[0]", path_str))
            .arg("-auto-orient")
            .arg("-resize")
            .arg(format!("{}x{}>", pixel_width, pixel_height))
            .arg("PNG:-")
            .output();
        let raw_data = match output {
            Ok(o) if !o.stdout.is_empty() => o.stdout,
            _ => continue,
        };

        // Pad to cell-aligned dims so the foreground show path's
        // `raw_w == pad_w && raw_h == pad_h` check skips its own
        // pad subprocess. Same convert invocation as the sync
        // pad path in kitty_display, just done off the hot path.
        let raw_w = png_width(&raw_data).unwrap_or(pixel_width);
        let raw_h = png_height(&raw_data).unwrap_or(pixel_height);
        let pad_w = if cell_w > 0 {
            ((raw_w + cell_w as u32 - 1) / cell_w as u32) * cell_w as u32
        } else { raw_w };
        let pad_h = if cell_h > 0 {
            ((raw_h + cell_h as u32 - 1) / cell_h as u32) * cell_h as u32
        } else { raw_h };

        let final_data = if pad_w == raw_w && pad_h == raw_h {
            raw_data
        } else {
            use std::io::Write;
            let mut child = match Command::new(imagemagick_cmd())
                .arg("PNG:-")
                .arg("-background").arg("rgba(0,0,0,0)")
                .arg("-gravity").arg("NorthWest")
                .arg("-extent").arg(format!("{}x{}", pad_w, pad_h))
                .arg("PNG:-")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn() {
                Ok(c) => c,
                Err(_) => continue,
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&raw_data);
            }
            match child.wait_with_output() {
                Ok(o) if !o.stdout.is_empty() => o.stdout,
                _ => continue,
            }
        };

        // Two-tier put: write to disk so the next kastrup launch can
        // reuse this padded PNG without re-running `convert`, AND
        // populate RAM with the same data so the foreground show
        // path is a single HashMap lookup away.
        cache_put(cache, key, final_data);
    }
}

/// RAII guard for DECSET 2026 (Synchronized Output Mode). Emits
/// `\x1b[?2026h` on construction and `\x1b[?2026l` on Drop. Use to
/// wrap any multi-write graphics sequence so the terminal renders
/// it as a single atomic frame.
///
/// The Drop is critical: glow's kitty_display has early-return
/// paths (cache miss + decode failure, etc.). Without the Drop the
/// closing `2026l` would be skipped on those paths and the terminal
/// would stay in sync mode — no further output would render until
/// the next `2026l` from somewhere else.
struct SyncOutput;
impl SyncOutput {
    fn begin() -> Self {
        print!("\x1b[?2026h");
        Self
    }
}
impl Drop for SyncOutput {
    fn drop(&mut self) {
        print!("\x1b[?2026l");
        io::stdout().flush().ok();
    }
}

/// Seed for the per-Display id counter. Picked from the wall clock
/// nanoseconds so two Display instances in the same process won't
/// reuse each other's ids if one shuts down and another starts.
/// Zero is never returned — kitty treats id=0 as "no id".
fn seed_id() -> u32 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(1);
    if n == 0 { 1 } else { n }
}

pub struct Display {
    protocol: Option<Protocol>,
    /// Where the last picture went on a bare console, so it can be
    /// taken away again: left, top, width, height, in pixels.
    fb_shown: Option<(i64, i64, usize, usize)>,
    /// The console screen, opened once and kept. Opening it again for
    /// every frame would cost an ioctl and a mapping each time.
    fb: Option<fb::Screen>,
    active_ids: Vec<u32>,
    image_cache: HashMap<String, (u32, u16, u16)>,  // (image_id, natural_pixel_w, natural_pixel_h)
    pub png_cache: PngCache,
    /// Monotonic per-Display id allocator. Used instead of a
    /// millisecond-timestamp so two rapid-fire kitty_display calls
    /// can't end up with the same id. The previous
    /// `SystemTime::now().as_millis() % 4G` formula produced
    /// collisions on rapid j/k navigation in pointer; when the
    /// fresh id happened to equal an `active_ids` entry, the
    /// delete-before-place path wiped the data we'd just
    /// transmitted, leaving a blank image until the user pressed
    /// Enter (which forced a re-render under a different ms-tick).
    next_id: u32,
}

impl Display {
    /// Auto-detect the best protocol
    pub fn new() -> Self {
        let protocol = detect_protocol();
        Self {
            protocol,
            fb_shown: None,
            fb: None,
            active_ids: Vec::new(),
            image_cache: HashMap::new(),
            png_cache: new_png_cache(),
            next_id: seed_id(),
        }
    }

    /// Force a specific display mode ("auto", "halfblock", "braille",
    /// "ascii", "chafa", "kitty", "sixel", "off"). The text modes need no
    /// graphics protocol, which makes them the way to show a picture over
    /// SSH or in tmux without passthrough.
    pub fn with_mode(mode: &str) -> Self {
        let protocol = match mode {
            "chafa" => {
                if command_exists("chafa") { Some(Protocol::Chafa) } else { None }
            }
            "ascii" | "text" => {
                // Best text rendering available: two colours per cell if
                // the terminal has truecolour, then chafa, then the plain
                // ramp, which is all the Linux console can take.
                if truecolor() { Some(Protocol::HalfBlock) }
                else if command_exists("chafa") { Some(Protocol::Chafa) }
                else if command_exists(imagemagick_cmd()) { Some(Protocol::Ascii) }
                else { None }
            }
            "halfblock" | "half" | "blocks" => Some(Protocol::HalfBlock),
            "kitty" => Some(Protocol::Kitty),
            "sixel" => Some(Protocol::Sixel),
            "braille" => Some(Protocol::Braille),
            "off" | "none" => None,
            _ => detect_protocol(), // "auto"
        };
        Self {
            protocol,
            fb_shown: None,
            fb: None,
            active_ids: Vec::new(),
            image_cache: HashMap::new(),
            png_cache: new_png_cache(),
            next_id: seed_id(),
        }
    }

    /// Allocate a fresh kitty image id. Strictly monotonic per
    /// Display, with a wrap that skips zero (kitty treats id=0 as
    /// "no id"). Also skips ids currently in `active_ids` so a
    /// brand-new transmit never collides with a live placement.
    fn allocate_id(&mut self) -> u32 {
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            if self.next_id == 0 { self.next_id = 1; }
            if !self.active_ids.contains(&self.next_id) { return self.next_id; }
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
            Protocol::HalfBlock => half_block_display(image_path, x, y, max_width, max_height),
            Protocol::Braille => braille_display(image_path, x, y, max_width, max_height),
            Protocol::Ascii => ascii_display(image_path, x, y, max_width, max_height),
            Protocol::Framebuffer => fb_display(image_path, x, y, max_width, max_height),
        }
    }

    /// Show a cropped vertical slice of an image. The image is sized for
    /// `(max_width, max_height)` cells (its full natural rendered dims),
    /// then only rows `[src_top_cells, src_top_cells + src_visible_cells)`
    /// are placed at screen `(x, y)`. Cache key uses `(max_width,
    /// max_height)` so scrolling an image into/out of a viewport
    /// reuses the same cached image_id — no fresh transmission, no
    /// new IMG_SLOT consumed in glass per scroll line. Used by scroll
    /// and other callers that page images at viewport edges.
    /// Falls back to non-clipped `show` for protocols other than kitty.
    pub fn show_clipped(&mut self, image_path: &str, x: u16, y: u16,
                        max_width: u16, max_height: u16,
                        src_top_cells: u16, src_visible_cells: u16) -> bool {
        let proto = match self.protocol {
            Some(p) => p,
            None => return false,
        };
        if !Path::new(image_path).exists() {
            return false;
        }
        match proto {
            Protocol::Kitty => self.kitty_display_clipped(
                image_path, x, y, max_width, max_height,
                src_top_cells, src_visible_cells),
            // Other protocols don't support source-rect cropping —
            // fall back to placing what fits.
            _ => self.show(image_path, x, y, max_width, src_visible_cells.max(1)),
        }
    }

    /// Show a PNG the caller made in memory, at cell (`x`, `y`), in a box
    /// of `width` × `height` cells. For a picture drawn fresh on every key
    /// press: under kitty the bytes go straight to the terminal, with no
    /// file and nothing added to the image caches, which would otherwise
    /// keep every frame. Size the PNG to whole cells (`get_cell_size`) and
    /// kitty places it without stretching. Other protocols read it from a
    /// temporary file through [`show`](Self::show). Call `clear` first
    /// when replacing an earlier picture, so the terminal can free it.
    pub fn show_png(&mut self, png: &[u8], x: u16, y: u16, width: u16, height: u16) -> bool {
        let Some(proto) = self.protocol else { return false };
        if proto == Protocol::Framebuffer {
            let Ok(picture) = image::load_from_memory(png) else { return false };
            return self.fb_put(&picture.to_rgba8(), x, y);
        }
        if proto != Protocol::Kitty {
            self.next_id = self.next_id.wrapping_add(1);
            let path = std::env::temp_dir()
                .join(format!("glow-{}-{}.png", std::process::id(), self.next_id));
            if std::fs::write(&path, png).is_err() { return false; }
            let ok = self.show(&path.to_string_lossy(), x, y, width, height);
            // w3m's helper reads the file after we return; the rest are done with it.
            if proto != Protocol::W3m { let _ = std::fs::remove_file(&path); }
            return ok;
        }
        let (cell_w, cell_h) = get_cell_size();
        if cell_w == 0 || cell_h == 0 { return false; }
        let (Some(w), Some(h)) = (png_width(png), png_height(png)) else { return false };
        let cols = w.div_ceil(cell_w as u32).clamp(1, width.max(1) as u32);
        let rows = h.div_ceil(cell_h as u32).clamp(1, height.max(1) as u32);
        let _sync = SyncOutput::begin();
        let id = self.allocate_id();
        kitty_transmit(id, png);
        print!("\x1b[{};{}H", y, x);
        let place = format!("\x1b_Ga=p,i={},c={},r={},z=1,q=2,C=1\x1b\\", id, cols, rows);
        // Placed twice, as in `kitty_display`: kitty can drop the first
        // place while the last chunk is still being assembled.
        print!("{}{}", place, place);
        io::stdout().flush().ok();
        self.active_ids.push(id);
        true
    }

    /// Delete just the placement(s) for `image_path` (per-id `a=d,d=i`).
    /// Lets callers do per-image diffs without nuking every active id —
    /// otherwise every line of scrolling burns fresh IMG_SLOTS for
    /// images that haven't actually changed. Match is by path prefix
    /// (cache key is `path:WxH:mtime`), so all entries for the same
    /// path get cleared together (covers width changes from pane resize).
    pub fn forget_path(&mut self, image_path: &str) {
        if !matches!(self.protocol, Some(Protocol::Kitty)) {
            // Only kitty has per-id placements; other protocols rely on
            // text redraw and have nothing to forget here.
            return;
        }
        let prefix = format!("{}:", image_path);
        let mut ids_to_forget: Vec<u32> = Vec::new();
        for (key, (id, _, _)) in &self.image_cache {
            if key.starts_with(&prefix) && self.active_ids.contains(id) {
                ids_to_forget.push(*id);
            }
        }
        if ids_to_forget.is_empty() { return; }
        for id in &ids_to_forget {
            print!("\x1b_Ga=d,d=i,i={},q=2\x1b\\", id);
        }
        io::stdout().flush().ok();
        self.active_ids.retain(|id| !ids_to_forget.contains(id));
    }

    /// Clear all displayed images
    /// Delete every placement the terminal is holding, ours or not, and
    /// forget our own bookkeeping.
    ///
    /// `clear` deletes the ids we think are on screen; this asks the
    /// terminal to drop them all. Use it on the way out: an app that
    /// exits with a stale id, or that never got to track one, otherwise
    /// leaves images painted over the user's shell — and once the process
    /// is gone nothing can clean them up.
    pub fn clear_all(&mut self) {
        if matches!(self.protocol, Some(Protocol::Kitty)) {
            // d=a: delete all placements, keep the transmitted data.
            print!("\x1b_Ga=d,d=a,q=2\x1b\\");
            io::stdout().flush().ok();
        }
        if let Some((px, py, w, h)) = self.fb_shown {
            if let Some(screen) = self.fb_screen() {
                screen.fill(px, py, w, h, (0, 0, 0));
            }
            self.fb_shown = None;
        }
        self.active_ids.clear();
    }

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
            Some(Protocol::HalfBlock) | Some(Protocol::Braille) => {
                // Both are text, cleared by the terminal's own redraw
            }
            Some(Protocol::Ascii) => {
                // ASCII is text-based, cleared by terminal redraw
            }
            Some(Protocol::Framebuffer) => {
                // Nothing redraws the console's pixels but us, so the
                // picture is painted out where it stood.
                if let Some((px, py, w, h)) = self.fb_shown {
                    if let Some(screen) = self.fb_screen() {
                        screen.fill(px, py, w, h, (0, 0, 0));
                    }
                }
                self.fb_shown = None;
            }
            None => {}
        }
    }

    // --- Kitty protocol ---

    /// Ensure image data for `(image_path, max_width, max_height)` is
    /// present server-side. Returns `(image_id, padded_pixel_w,
    /// padded_pixel_h, cell_w, cell_h)`. Cache lookup is by
    /// `(path, pixel_w, pixel_h, mtime)` — callers wanting stable
    /// cache hits across viewport-edge clipping should pass the FULL
    /// natural rendered size (not the visible-portion size).
    fn kitty_ensure(&mut self, image_path: &str, max_width: u16, max_height: u16)
        -> Option<(u32, u16, u16, u16, u16)>
    {
        let (cell_w, cell_h) = get_cell_size();
        if cell_w == 0 || cell_h == 0 { return None; }
        let pixel_w = max_width as u32 * cell_w as u32;
        let pixel_h = max_height as u32 * cell_h as u32;
        let mtime = std::fs::metadata(image_path)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0);
        let cache_key = format!("{}:{}x{}:{}{}", image_path, pixel_w, pixel_h, mtime, if is_svg(image_path) { ":rsvg" } else { "" });
        let cached_live = self.image_cache.get(&cache_key)
            .filter(|(id, _, _)| self.active_ids.contains(id))
            .copied();
        if let Some((id, pw, ph)) = cached_live {
            return Some((id, pw, ph, cell_w, cell_h));
        }
        // Cache miss for this (path, w, h). Forget any other live
        // placements of the same path (different cache_keys from
        // earlier non-clipped sizes) — otherwise they ride the DEC
        // scroll region as ghosts.
        self.forget_path(image_path);
        let id = self.allocate_id();
        // Two-tier lookup: RAM, then on-disk PNG cache. Falls through
        // to `convert` only when neither tier has the resized PNG.
        let png_data = match cache_get(&self.png_cache, &cache_key) {
            Some(data) => data,
            None => {
                let output = raster_png(image_path, pixel_w, pixel_h);
                let data = match output {
                    Ok(o) if !o.stdout.is_empty() => o.stdout,
                    _ => return None,
                };
                cache_put(&self.png_cache, cache_key.clone(), data.clone());
                data
            }
        };
        let raw_h = png_height(&png_data).unwrap_or(pixel_w);
        let raw_w = png_width(&png_data).unwrap_or(pixel_w);
        let pad_w = ((raw_w + cell_w as u32 - 1) / cell_w as u32) * cell_w as u32;
        let pad_h = ((raw_h + cell_h as u32 - 1) / cell_h as u32) * cell_h as u32;
        let png_data = if pad_w == raw_w && pad_h == raw_h {
            png_data
        } else {
            use std::io::Write;
            let mut child = match Command::new(imagemagick_cmd())
                .arg("PNG:-")
                .arg("-background")
                .arg("rgba(0,0,0,0)")
                .arg("-gravity")
                .arg("NorthWest")
                .arg("-extent")
                .arg(format!("{}x{}", pad_w, pad_h))
                .arg("PNG:-")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn() {
                Ok(c) => c,
                Err(_) => return None,
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&png_data);
            }
            match child.wait_with_output() {
                Ok(o) if !o.stdout.is_empty() => o.stdout,
                _ => return None,
            }
        };
        // Cache the cell-aligned PNG (see kitty_display for full
        // rationale). On revisit, the pad subprocess is skipped
        // because raw_w/raw_h read from this cached PNG already
        // equal pad_w/pad_h.
        cache_put(&self.png_cache, cache_key.clone(), png_data.clone());
        let img_pixel_w = pad_w as u16;
        let img_pixel_h = pad_h as u16;
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
        self.image_cache.insert(cache_key, (id, img_pixel_w, img_pixel_h));
        if !self.active_ids.contains(&id) {
            self.active_ids.push(id);
        }
        Some((id, img_pixel_w, img_pixel_h, cell_w, cell_h))
    }

    fn kitty_display_clipped(&mut self, image_path: &str, x: u16, y: u16,
                             max_width: u16, max_height: u16,
                             src_top_cells: u16, src_visible_cells: u16) -> bool {
        let (image_id, pad_w, pad_h, cell_w, cell_h) =
            match self.kitty_ensure(image_path, max_width, max_height) {
                Some(t) => t,
                None => return false,
            };
        let visible = src_visible_cells.max(1);
        let src_y_px = (src_top_cells as u32 * cell_h as u32).min(pad_h as u32);
        let src_h_px = (visible as u32 * cell_h as u32).min(pad_h as u32 - src_y_px);
        if src_h_px == 0 { return false; }
        // Move existing placement (per-id delete) then place at new
        // position with source-rect crop. Same image_id is reused
        // every scroll line — no re-transmit, no IMG_SLOT churn.
        //
        // Skip the delete on the cache-miss path (no prior placement
        // yet) — see kitty_display for the full rationale on the
        // delete-before-place race.
        let already_placed = self.active_ids.contains(&image_id);
        if already_placed {
            print!("\x1b_Ga=d,d=i,i={},q=2\x1b\\", image_id);
        }
        print!("\x1b[{};{}H", y, x);
        let cols = (pad_w as u32 / cell_w as u32).max(1) as u16;
        let rows = (src_h_px / cell_h as u32).max(1) as u16;
        // Lowercase x,y,w,h in place command = source-rect crop in pixels.
        print!("\x1b_Ga=p,i={},x=0,y={},w={},h={},c={},r={},z=1,q=2,C=1\x1b\\",
            image_id, src_y_px, pad_w, src_h_px, cols, rows);
        io::stdout().flush().ok();
        if !already_placed {
            self.active_ids.push(image_id);
        }
        true
    }

    fn kitty_display(&mut self, image_path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
        let (cell_w, cell_h) = get_cell_size();
        if cell_w == 0 || cell_h == 0 {
            return false;
        }

        // Wrap the entire transmit + place sequence in DECSET 2026
        // (Synchronized Output). The terminal buffers everything
        // between `\x1b[?2026h` and `\x1b[?2026l` and renders once
        // at the close, so neighbour text refreshes can't briefly
        // occlude the placement and there's no in-between frame
        // where the new image is partially placed.
        //
        // Drop guard ensures the closing `2026l` always fires —
        // including the cache-miss-and-decode-fails early returns
        // below. Without the guard those paths would leave the
        // terminal in sync mode forever (no further output would
        // render until the next 2026l from somewhere).
        let _sync = SyncOutput::begin();

        let pixel_w = max_width as u32 * cell_w as u32;
        let pixel_h = max_height as u32 * cell_h as u32;

        // Cache by path + width + height + mtime. Including height matters
        // because a tall image may have to be height-clamped on a short
        // pane and width-clamped on a wide pane — different convert outputs.
        let mtime = std::fs::metadata(image_path)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0);
        let cache_key = format!("{}:{}x{}:{}{}", image_path, pixel_w, pixel_h, mtime, if is_svg(image_path) { ":rsvg" } else { "" });

        // Cache hit only counts if the image is still considered "live"
        // server-side. Once clear() deletes the only placement of an image
        // id, kitty frees the image data, so a place command for that id
        // would silently fail. active_ids is empty after clear(), so a
        // cached id missing from active_ids signals "data may be gone" —
        // fall through and re-transmit.
        let cached_live = self.image_cache.get(&cache_key)
            .filter(|(id, _, _)| self.active_ids.contains(id))
            .copied();
        let (image_id, nat_pixel_w, nat_pixel_h) = if let Some(cached) = cached_live {
            cached
        } else {
            // Cache miss for this (path, w, h). If we have other live
            // placements of the same path under a different cache_key
            // (typically: caller varied max_height as the image clipped
            // at viewport edges), kill them first. Otherwise the per-id
            // delete below is a no-op for those stale ids and they keep
            // riding the DEC scroll region as ghost duplicates.
            self.forget_path(image_path);
            let id = self.allocate_id();

            // Two-tier cache: in-RAM first, fall back to the on-disk
            // PNG cache populated by previous runs. Either path skips
            // image processing entirely.
            //
            // Cache miss → Phase 2 Rust pipeline (`image` crate)
            // does decode + resize + cell-aligned pad in one pass.
            // Falls through to the `magick` subprocess only when the
            // Rust pipeline can't decode (HEIC, SVG, weird CMYK
            // JPEG, etc.).
            let png_data = match cache_get(&self.png_cache, &cache_key) {
                Some(data) => data,
                None => {
                    // Phase 2: Rust-side resize + pad.
                    if let Some(data) = rust_resize_and_pad(
                        image_path, pixel_w, pixel_h,
                        cell_w as u32, cell_h as u32,
                    ) {
                        cache_put(&self.png_cache, cache_key.clone(), data.clone());
                        data
                    } else {
                        // Magick fallback: resize, then optional pad.
                        let output = raster_png(image_path, pixel_w, pixel_h);
                        let raw_data = match output {
                            Ok(o) if !o.stdout.is_empty() => o.stdout,
                            _ => return false,
                        };
                        let raw_h = png_height(&raw_data).unwrap_or(pixel_w);
                        let raw_w = png_width(&raw_data).unwrap_or(pixel_w);
                        let pad_w = ((raw_w + cell_w as u32 - 1) / cell_w as u32) * cell_w as u32;
                        let pad_h = ((raw_h + cell_h as u32 - 1) / cell_h as u32) * cell_h as u32;
                        let final_data = if pad_w == raw_w && pad_h == raw_h {
                            raw_data
                        } else {
                            use std::io::Write;
                            let mut child = match Command::new(imagemagick_cmd())
                                .arg("PNG:-")
                                .arg("-background").arg("rgba(0,0,0,0)")
                                .arg("-gravity").arg("NorthWest")
                                .arg("-extent").arg(format!("{}x{}", pad_w, pad_h))
                                .arg("PNG:-")
                                .stdin(std::process::Stdio::piped())
                                .stdout(std::process::Stdio::piped())
                                .stderr(std::process::Stdio::null())
                                .spawn() {
                                Ok(c) => c,
                                Err(_) => return false,
                            };
                            if let Some(mut stdin) = child.stdin.take() {
                                let _ = stdin.write_all(&raw_data);
                            }
                            match child.wait_with_output() {
                                Ok(o) if !o.stdout.is_empty() => o.stdout,
                                _ => return false,
                            }
                        };
                        cache_put(&self.png_cache, cache_key.clone(), final_data.clone());
                        final_data
                    }
                }
            };

            // Read the actual padded dims from the PNG header — either
            // tier returns already-padded data, so raw_w/raw_h == pad_w/pad_h.
            let raw_h = png_height(&png_data).unwrap_or(pixel_w);
            let raw_w = png_width(&png_data).unwrap_or(pixel_w);
            let pad_w = ((raw_w + cell_w as u32 - 1) / cell_w as u32) * cell_w as u32;
            let pad_h = ((raw_h + cell_h as u32 - 1) / cell_h as u32) * cell_h as u32;

            let img_pixel_w = pad_w;
            let img_pixel_h = pad_h;

            // Chunked base64 transmit (legacy path, known-working).
            // The t=f / file-path transmit attempted in v0.1.17 broke
            // image display entirely — under investigation. Keep this
            // path until we've verified t=f works flawlessly.
            kitty_transmit(id, &png_data);
            // Cache the actual PNG pixel dims (not cell-rounded) so the
            // fit/scale math below operates on truth, not on the
            // cell-aligned approximation. With 12-px cells a 241-px
            // wide image rounds up to 252 px, which spuriously trips
            // needs_shrink for any pane <252 px and ends up stretching
            // the image vertically.
            self.image_cache.insert(cache_key, (id, img_pixel_w as u16, img_pixel_h as u16));
            (id, img_pixel_w as u16, img_pixel_h as u16)
        };

        // Delete previous placement (only if one exists) then place at
        // new position with z=1.
        //
        // The delete must NOT fire on the cache-miss path: there we
        // just transmitted fresh data with no placement yet, so a
        // `d=i` would tell kitty "image X has zero placements" — and
        // depending on timing kitty may free the data before the
        // following place command attaches. The 1-in-20 silent
        // "image doesn't show until I press Enter" race lived here.
        //
        // z=1 puts the image above pane text so concurrent terminal
        // redraws (e.g. neighbouring tiled window expose events)
        // cannot overdraw cells and hide the image — a kitty +
        // tiled-WM bug that otherwise required a workspace switch
        // to recover.
        let already_placed = self.active_ids.contains(&image_id);
        if already_placed {
            print!("\x1b_Ga=d,d=i,i={},q=2\x1b\\", image_id);
        }
        print!("\x1b[{};{}H", y, x);

        // The transmitted PNG is already padded to cell-aligned dims
        // (raw image content top-left, transparent fill below/right).
        // So c×cell_w and r×cell_h match the PNG's pixel dims exactly,
        // and kitty doesn't stretch.
        let cols = (nat_pixel_w as u32 / cell_w as u32).max(1) as u16;
        let rows = (nat_pixel_h as u32 / cell_h as u32).max(1) as u16;
        let place = format!("\x1b_Ga=p,i={},c={},r={},z=1,q=2,C=1\x1b\\",
            image_id, cols, rows);
        // Double-tap the place command. On rapid navigation (pointer's
        // j/k through an image directory), kitty occasionally drops
        // the first place silently — the chunked transmit hasn't
        // finished assembling server-side when the place arrives, so
        // it references data that isn't yet ready. The second place,
        // emitted right after, lands after kitty has processed the
        // transmit chunks and lights up the placement. Cheap
        // insurance: ~30 extra bytes per show, idempotent if the
        // first placement actually succeeded.
        print!("{}{}", place, place);
        io::stdout().flush().ok();

        if !already_placed {
            self.active_ids.push(image_id);
        }
        true
    }
}

/// Send PNG bytes to a kitty terminal as image `id`, base64 in 4096-byte
/// chunks, without placing it.
/// True inside glass, which sets `_GLASS_ID` for its children. glass
/// takes raw pixels over shared memory, so no PNG has to be decoded by
/// a forked ImageMagick on its side and no base64 crosses the pty.
fn in_glass() -> bool {
    std::env::var_os("_GLASS_ID").is_some()
}

static SHM_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Write `bytes` to a fresh shared-memory file and return its name, the
/// way the kitty protocol's `t=s` wants it. A ring of names per process
/// keeps a frame from landing on a file the terminal has not read yet.
fn shm_write(bytes: &[u8], ring: u32) -> Option<String> {
    let n = SHM_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % ring.max(1);
    let name = format!("glow-{}-{}", std::process::id(), n);
    std::fs::write(format!("/dev/shm/{}", name), bytes).ok()?;
    Some(name)
}

/// The transmit for a PNG inside glass: decode it here, hand the pixels
/// over shared memory. None when the PNG does not decode.
fn kitty_transmit_shm(id: u32, png: &[u8]) -> Option<String> {
    let img = image::load_from_memory(png).ok()?;
    let (w, h) = (img.width(), img.height());
    let (fmt, name) = if img.color().has_alpha() {
        (32, shm_write(img.to_rgba8().as_raw(), 64)?)
    } else {
        (24, shm_write(img.to_rgb8().as_raw(), 64)?)
    };
    let payload = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
    Some(format!("\x1b_Ga=t,f={},t=s,i={},s={},v={},q=2;{}\x1b\\", fmt, id, w, h, payload))
}

fn kitty_transmit(id: u32, png: &[u8]) {
    if in_glass() {
        if let Some(seq) = kitty_transmit_shm(id, png) {
            print!("{}", seq);
            io::stdout().flush().ok();
            return;
        }
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
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
}

// --- Sixel protocol ---

fn sixel_display(image_path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
    let pixel_w = max_width as u32 * 10;
    let pixel_h = max_height as u32 * 20;
    print!("\x1b[{};{}H", y, x);
    io::stdout().flush().ok();
    // No shell here: Command passes argv straight to execve, so a
    // shell-quoted path would arrive with its quotes as part of the name
    // ("no decode delegate for `'/path/x.png''").
    Command::new(imagemagick_cmd())
        .arg(image_path)
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

    // Get image dimensions (raw path: no shell in the way, see above)
    let dims = Command::new("identify")
        .arg("-format")
        .arg("%wx%h")
        .arg(format!("{}[0]", image_path))
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
    let size = format!("{}x{}", max_width, max_height);
    let base = [
        "--size", &size,
        "--animate", "off",
        "--format", "symbols",  // Force text symbols, not sixel/kitty
        "--color-space", "din99d",
    ];
    let run = |extra: &[&str]| {
        Command::new("chafa").args(base).args(extra).arg(image_path).output()
    };
    // Left alone, chafa asks the terminal what it can do and waits up to
    // five seconds for an answer. A terminal that never replies costs
    // exactly those five seconds, every image. We already decided the
    // format, so there is nothing to ask. Older chafa has no --probe, so
    // fall back to a plain run if that fails.
    let output = match run(&["--probe", "off"]) {
        Ok(o) if o.status.success() => Ok(o),
        _ => run(&[]),
    };
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

fn detect_protocol() -> Option<Protocol> {
    // Kitty
    if std::env::var("TERM").unwrap_or_default() == "xterm-kitty"
        || std::env::var("KITTY_WINDOW_ID").is_ok()
        || std::env::var("TERM_PROGRAM").unwrap_or_default() == "WezTerm"
        || std::env::var("WEZTERM_EXECUTABLE").is_ok()
    {
        if command_exists(imagemagick_cmd()) {
            return Some(Protocol::Kitty);
        }
    }

    // Sixel (xterm, mlterm, foot)
    let term = std::env::var("TERM").unwrap_or_default();
    if term.starts_with("xterm") && term != "xterm-kitty" || term.starts_with("mlterm") || term.starts_with("foot") {
        if command_exists(imagemagick_cmd()) {
            return Some(Protocol::Sixel);
        }
    }

    // W3m fallback — draws into an X11 window, so it is useless without a
    // running display. In a bare TTY (`DISPLAY`/`WAYLAND_DISPLAY` unset) the
    // helper binaries can still be installed; detecting W3m there swallows
    // the text fallbacks below and shows NOTHING. Gate on a live display.
    let has_display = std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("WAYLAND_DISPLAY").is_some();
    if has_display && Path::new("/usr/lib/w3m/w3mimgdisplay").exists() {
        if command_exists("xwininfo") && command_exists("xdotool") && command_exists("identify") {
            return Some(Protocol::W3m);
        }
    }

    // A bare console has no protocol at all, but it has the screen.
    // Real pixels beat any arrangement of characters, so this comes
    // before the text fallbacks and after everything a terminal offers.
    if !has_display && fb::there() {
        return Some(Protocol::Framebuffer);
    }

    // Text fallbacks, in order of how good they look.
    //
    // Half blocks come first: two full-colour pixels per cell, decoded
    // and scaled in this process, so no fork per image and nothing to
    // install. Then chafa, which is a fork but knows more glyphs than we
    // do. Then braille, which packs 2×4 dots into one colour. The Linux
    // console has neither truecolour nor a U+2800 block in its font, so
    // it gets the plain ramp.
    if truecolor() {
        return Some(Protocol::HalfBlock);
    }
    if command_exists("chafa") {
        return Some(Protocol::Chafa);
    }
    if std::env::var("TERM").unwrap_or_default() == "linux" {
        return Some(Protocol::Ascii);
    }
    Some(Protocol::Braille)
}

/// Does this terminal take 24-bit colour? The half-block renderer is
/// pointless without it, and every terminal that has it says so.
fn truecolor() -> bool {
    let ct = std::env::var("COLORTERM").unwrap_or_default();
    if ct.contains("truecolor") || ct.contains("24bit") {
        return true;
    }
    let term = std::env::var("TERM").unwrap_or_default();
    term.contains("kitty") || term.contains("direct") || term.contains("alacritty")
}

// --- Shared pixel sampling for the text fallbacks ---

/// sRGB byte to linear light, scaled to 0..65535. Averaging pixels in
/// this space is what keeps a shrunken image at the brightness it
/// started with: sRGB values are perceptual, and adding them directly
/// makes every downscale come out dark.
fn srgb_to_linear() -> &'static [u32; 256] {
    static LUT: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let c = i as f64 / 255.0;
            let lin = if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
            *slot = (lin * 65535.0).round() as u32;
        }
        t
    })
}

/// And back again, from 0..65535 linear to an sRGB byte. One entry per
/// linear step (64 KB, built once on first use): a coarser table rounds
/// the darkest few values to zero, and dark detail is exactly where the
/// eye notices.
fn linear_to_srgb(v: u32) -> u8 {
    static LUT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let lut = LUT.get_or_init(|| {
        (0..=65535u32)
            .map(|i| {
                let lin = i as f64 / 65535.0;
                let c = if lin <= 0.0031308 {
                    lin * 12.92
                } else {
                    1.055 * lin.powf(1.0 / 2.4) - 0.055
                };
                (c * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect()
    });
    lut[v.min(65535) as usize]
}

/// Decode `path` and box-average it down to at most `max_w` × `max_h`
/// pixels, keeping the aspect ratio. Returns `(w, h, RGBA8)`.
///
/// The averaging happens in linear light, so a checkerboard of black and
/// white shrinks to the grey it physically is rather than the darker one
/// that adding sRGB bytes would give.
fn pixel_grid(path: &str, max_w: u32, max_h: u32) -> Option<(u32, u32, Vec<u8>)> {
    if max_w == 0 || max_h == 0 {
        return None;
    }
    if is_svg(path) && have_rsvg() {
        let png = raster_png(path, max_w, max_h).ok()?.stdout;
        let img = image::load_from_memory(&png).ok()?.to_rgba8();
        return Some((img.width(), img.height(), img.into_raw()));
    }
    match pixel_grid_native(path, max_w, max_h) {
        Some(g) => Some(g),
        // HEIC, odd CMYK JPEGs: whatever the `image` crate will not
        // decode, ImageMagick still can.
        None => pixel_grid_magick(path, max_w, max_h),
    }
}

/// The dimensions an image of `sw` × `sh` takes inside the box, with
/// square pixels.
fn fit_within(sw: u32, sh: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f64 / sw as f64).min(max_h as f64 / sh as f64);
    (
        ((sw as f64 * scale).round() as u32).clamp(1, max_w),
        ((sh as f64 * scale).round() as u32).clamp(1, max_h),
    )
}

fn pixel_grid_native(path: &str, max_w: u32, max_h: u32) -> Option<(u32, u32, Vec<u8>)> {
    use image::ImageReader;
    let img = ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?;
    let src = img.to_rgba8();
    let (sw, sh) = (src.width(), src.height());
    if sw == 0 || sh == 0 {
        return None;
    }
    let (tw, th) = fit_within(sw, sh, max_w, max_h);
    Some((tw, th, box_average(src.as_raw(), sw, sh, tw, th)))
}

/// Average each target pixel over the source rectangle it covers, in
/// linear light, weighting colour by alpha so transparent pixels do not
/// bleed their colour into their neighbours. Upscaling falls back to
/// picking the nearest source pixel.
fn box_average(src: &[u8], sw: u32, sh: u32, tw: u32, th: u32) -> Vec<u8> {
    let lin = srgb_to_linear();
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for ty in 0..th {
        let y0 = (ty as u64 * sh as u64 / th as u64) as u32;
        let y1 = (((ty + 1) as u64 * sh as u64 / th as u64) as u32).max(y0 + 1).min(sh);
        for tx in 0..tw {
            let x0 = (tx as u64 * sw as u64 / tw as u64) as u32;
            let x1 = (((tx + 1) as u64 * sw as u64 / tw as u64) as u32).max(x0 + 1).min(sw);
            let (mut r, mut g, mut b, mut a, mut aw) = (0u64, 0u64, 0u64, 0u64, 0u64);
            let mut n = 0u64;
            for sy in y0..y1 {
                let row = (sy * sw * 4) as usize;
                for sx in x0..x1 {
                    let i = row + (sx * 4) as usize;
                    let al = src[i + 3] as u64;
                    r += lin[src[i] as usize] as u64 * al;
                    g += lin[src[i + 1] as usize] as u64 * al;
                    b += lin[src[i + 2] as usize] as u64 * al;
                    a += al;
                    aw += al;
                    n += 1;
                }
            }
            let o = ((ty * tw + tx) * 4) as usize;
            if aw == 0 || n == 0 {
                out[o + 3] = 0;
                continue;
            }
            out[o] = linear_to_srgb((r / aw) as u32);
            out[o + 1] = linear_to_srgb((g / aw) as u32);
            out[o + 2] = linear_to_srgb((b / aw) as u32);
            out[o + 3] = (a / n) as u8;
        }
    }
    out
}

/// The same, through ImageMagick, for formats the Rust decoder does not
/// know. `-colorspace RGB` before the resize and back after is IM's way
/// of saying "average in linear light".
fn pixel_grid_magick(path: &str, max_w: u32, max_h: u32) -> Option<(u32, u32, Vec<u8>)> {
    let info = Command::new(imagemagick_cmd())
        .arg(format!("{}[0]", path))
        .arg("-format").arg("%w %h")
        .arg("info:-")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&info.stdout);
    let mut it = s.split_whitespace();
    let sw: u32 = it.next()?.parse().ok()?;
    let sh: u32 = it.next()?.parse().ok()?;
    if sw == 0 || sh == 0 {
        return None;
    }
    let (tw, th) = fit_within(sw, sh, max_w, max_h);
    let raw = Command::new(imagemagick_cmd())
        .arg(format!("{}[0]", path))
        .arg("-auto-orient")
        .arg("-colorspace").arg("RGB")
        .arg("-resize").arg(format!("{}x{}!", tw, th))
        .arg("-colorspace").arg("sRGB")
        .arg("-depth").arg("8")
        .arg("RGBA:-")
        .output()
        .ok()?;
    if raw.stdout.len() != (tw * th * 4) as usize {
        return None;
    }
    Some((tw, th, raw.stdout))
}

/// A 4×2 ordered dither, as thresholds on 0..255. Used to turn a smooth
/// image into braille dots without losing the mid-tones a fixed
/// threshold throws away.
const BAYER_4X2: [[u8; 2]; 4] = [[16, 144], [208, 80], [48, 176], [240, 112]];

/// Perceptual brightness, the usual green-heavy weights.
fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 77 + g as u32 * 150 + b as u32 * 29) >> 8) as u8
}

/// Render `path` with half-block glyphs at (x, y), fitting within
/// `max_width` × `max_height` cells.
///
/// A cell gets `▀`: the foreground colour paints the top half, the
/// background the bottom. That is two full-colour pixels per cell, and
/// since a cell is about twice as tall as it is wide, both come out
/// square. It is the best a terminal can do without a graphics protocol,
/// and it needs no external program at all.
fn half_block_display(path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
    let (w, h, px) = match pixel_grid(path, max_width as u32, max_height as u32 * 2) {
        Some(g) => g,
        None => return false,
    };
    let out = half_block_frame(w, h, &px, x, y);
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stdout().flush();
    true
}

/// The escape sequence for a half-block frame, built whole so it can be
/// written in one go (and checked in a test).
fn half_block_frame(w: u32, h: u32, px: &[u8], x: u16, y: u16) -> String {
    let rows = h.div_ceil(2);
    let at = |cx: u32, cy: u32| -> Option<(u8, u8, u8)> {
        if cy >= h {
            return None;
        }
        let i = ((cy * w + cx) * 4) as usize;
        // Anything close to transparent shows the terminal through.
        if px[i + 3] < 64 { None } else { Some((px[i], px[i + 1], px[i + 2])) }
    };

    let mut out = String::with_capacity((w * rows) as usize * 24);
    for ry in 0..rows {
        out.push_str(&format!("\x1b[{};{}H", y as u32 + ry, x));
        // Colours are only re-sent when they change: a photo with large
        // flat areas emits a fraction of the bytes it otherwise would.
        let (mut cur_fg, mut cur_bg): (Option<(u8, u8, u8)>, Option<(u8, u8, u8)>) = (None, None);
        let mut open = false;
        for cx in 0..w {
            let top = at(cx, ry * 2);
            let bot = at(cx, ry * 2 + 1);
            match (top, bot) {
                (None, None) => {
                    if open {
                        out.push_str("\x1b[0m");
                        open = false;
                        cur_fg = None;
                        cur_bg = None;
                    }
                    out.push(' ');
                }
                // One half transparent: draw the other half as a glyph on
                // the terminal's own background.
                (Some(c), None) | (None, Some(c)) => {
                    if cur_bg.is_some() {
                        out.push_str("\x1b[49m");
                        cur_bg = None;
                    }
                    if cur_fg != Some(c) {
                        out.push_str(&format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2));
                        cur_fg = Some(c);
                    }
                    out.push(if top.is_some() { '▀' } else { '▄' });
                    open = true;
                }
                (Some(t), Some(b)) => {
                    if cur_fg != Some(t) {
                        out.push_str(&format!("\x1b[38;2;{};{};{}m", t.0, t.1, t.2));
                        cur_fg = Some(t);
                    }
                    if cur_bg != Some(b) {
                        out.push_str(&format!("\x1b[48;2;{};{};{}m", b.0, b.1, b.2));
                        cur_bg = Some(b);
                    }
                    out.push('▀');
                    open = true;
                }
            }
        }
        if open {
            out.push_str("\x1b[0m");
        }
    }
    out
}

/// Render `path` into Unicode braille glyphs at (x, y), fitting within
/// `max_width` × `max_height` character cells. Each cell is 2×4 dots, so
/// the effective resolution is (2·max_width) × (4·max_height), with one
/// colour per cell.
///
/// Which dots light is decided by an ordered dither rather than a fixed
/// brightness cut. A cut loses every mid-tone — a bright photo comes out
/// empty and a dark one solid — while dithering lets the density of lit
/// dots track the actual brightness, which is what makes eight dots per
/// cell worth having.
fn braille_display(path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
    let (w, h, px) = match pixel_grid(path, max_width as u32 * 2, max_height as u32 * 4) {
        Some(g) => g,
        None => return false,
    };
    let out = braille_frame(w, h, &px, x, y);
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stdout().flush();
    true
}

/// One braille cell: which dots light, and the colour of the ones that
/// did. Split out from the frame so the dither can be tested.
fn braille_cell(w: u32, h: u32, px: &[u8], cx: u32, cy: u32) -> (u32, Option<(u8, u8, u8)>) {
    // Braille dot bits: column-major, top to bottom.
    const DOT_BITS: [(u32, u32, u32); 8] = [
        (0, 0, 0x01), (0, 1, 0x02), (0, 2, 0x04), (0, 3, 0x40),
        (1, 0, 0x08), (1, 1, 0x10), (1, 2, 0x20), (1, 3, 0x80),
    ];
    let mut mask: u32 = 0;
    let (mut r_sum, mut g_sum, mut b_sum, mut lit) = (0u32, 0u32, 0u32, 0u32);
    for (dx, dy, bit) in &DOT_BITS {
        let (sx, sy) = (cx * 2 + dx, cy * 4 + dy);
        if sx >= w || sy >= h {
            continue;
        }
        let i = ((sy * w + sx) * 4) as usize;
        let (r, g, b, a) = (px[i], px[i + 1], px[i + 2], px[i + 3]);
        if a > 64 && luma(r, g, b) > BAYER_4X2[*dy as usize][*dx as usize] {
            mask |= bit;
            r_sum += r as u32;
            g_sum += g as u32;
            b_sum += b as u32;
            lit += 1;
        }
    }
    if lit == 0 {
        return (0, None);
    }
    // The cell takes the colour of the dots that lit, so a glyph shows
    // what is actually there rather than a wash of the neighbourhood.
    (mask, Some(((r_sum / lit) as u8, (g_sum / lit) as u8, (b_sum / lit) as u8)))
}

fn braille_frame(w: u32, h: u32, px: &[u8], x: u16, y: u16) -> String {
    let cells_w = w.div_ceil(2);
    let cells_h = h.div_ceil(4);

    let mut out = String::with_capacity((cells_w * cells_h) as usize * 24);
    for cy in 0..cells_h {
        out.push_str(&format!("\x1b[{};{}H", y as u32 + cy, x));
        let mut cur: Option<(u8, u8, u8)> = None;
        for cx in 0..cells_w {
            let (mask, colour) = braille_cell(w, h, px, cx, cy);
            match colour {
                None => {
                    if cur.is_some() {
                        out.push_str("\x1b[39m");
                        cur = None;
                    }
                    out.push(' ');
                }
                Some(c) => {
                    if cur != Some(c) {
                        out.push_str(&format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2));
                        cur = Some(c);
                    }
                    out.push(char::from_u32(0x2800 + mask).unwrap_or(' '));
                }
            }
        }
        if cur.is_some() {
            out.push_str("\x1b[39m");
        }
    }
    out
}

/// Plain-ASCII fallback renderer. Maps image luminance to a 10-level ramp,
/// one character per terminal cell, positioned with cursor moves. No color
/// (the Linux console has no truecolor) and no special glyphs, so it renders
/// on any font — including the sparse Linux virtual-console font where
/// braille shows blank. `convert`/`magick` only.
fn ascii_display(path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
    if max_width == 0 || max_height == 0 { return false; }

    // Original pixel dims (for aspect-preserving fit).
    let info = Command::new(imagemagick_cmd())
        .arg(format!("{}[0]", path))
        .arg("-format").arg("%w %h")
        .arg("info:-")
        .output();
    let (orig_w, orig_h) = match info {
        Ok(o) if !o.stdout.is_empty() => {
            let s = String::from_utf8_lossy(&o.stdout);
            let mut it = s.split_whitespace();
            let w: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let h: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            (w, h)
        }
        _ => return false,
    };
    if orig_w == 0 || orig_h == 0 { return false; }

    // One glyph per cell. A cell is ~2× taller than wide, so halve the row
    // count relative to square sampling to keep the image's aspect ratio.
    let mut cells_w = max_width as u32;
    let mut cells_h = ((cells_w * orig_h) as f64 / (orig_w as f64 * 2.0)).round() as u32;
    if cells_h > max_height as u32 {
        cells_h = max_height as u32;
        cells_w = ((cells_h * 2 * orig_w) as f64 / orig_h as f64).round() as u32;
    }
    cells_w = cells_w.clamp(1, max_width as u32);
    cells_h = cells_h.clamp(1, max_height as u32);

    // Grayscale, one byte per pixel, exactly cells_w × cells_h.
    let raw = Command::new(imagemagick_cmd())
        .arg(format!("{}[0]", path))
        .arg("-auto-orient")
        .arg("-resize").arg(format!("{}x{}!", cells_w, cells_h))
        .arg("-colorspace").arg("Gray")
        .arg("-depth").arg("8")
        .arg("GRAY:-")
        .output();
    let bytes = match raw {
        Ok(o) if o.stdout.len() == (cells_w as usize) * (cells_h as usize) => o.stdout,
        _ => return false,
    };

    // Dark → light. Space for the darkest, dense glyph for the brightest, so
    // on the console's black background more light reads as more ink.
    const RAMP: &[u8] = b" .:-=+*#%@";
    let last = (RAMP.len() - 1) as u32;

    let mut out = String::with_capacity((cells_w as usize + 8) * cells_h as usize);
    for cy in 0..cells_h {
        out.push_str(&format!("\x1b[{};{}H", y as u32 + cy, x));
        for cx in 0..cells_w {
            let lum = bytes[(cy * cells_w + cx) as usize] as u32;
            let idx = (lum * last / 255) as usize;
            out.push(RAMP[idx] as char);
        }
    }
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stdout().flush();
    true
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

/// Pick the ImageMagick CLI binary name. IM7 prefers `magick`; the
/// legacy `convert` symlink prints a deprecation warning on some
/// distros (Arch/Endeavour) which then bleeds onto the user's
/// terminal during a render pass. Resolved once per process.
/// The file rendered to a PNG that fits in `max_w` × `max_h`, without
/// enlarging a raster. An SVG goes through rsvg-convert when it is
/// installed: ImageMagick's own SVG renderer drops gradients and
/// misplaces text. Everything else goes through ImageMagick.
fn raster_png(path: &str, max_w: u32, max_h: u32) -> std::io::Result<std::process::Output> {
    if is_svg(path) && have_rsvg() {
        return Command::new("rsvg-convert")
            .arg("-w").arg(max_w.to_string())
            .arg("-h").arg(max_h.to_string())
            .arg("--keep-aspect-ratio")
            .arg("-f").arg("png")
            .arg(path)
            .output();
    }
    Command::new(imagemagick_cmd())
        .arg(format!("{}[0]", path))
        .arg("-auto-orient")
        .arg("-resize")
        .arg(format!("{}x{}>", max_w, max_h))
        .arg("PNG:-")
        .output()
}

fn is_svg(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".svg") || p.ends_with(".svgz")
}

fn have_rsvg() -> bool {
    static HAVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAVE.get_or_init(|| command_exists("rsvg-convert"))
}

fn imagemagick_cmd() -> &'static str {
    static CHOICE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    CHOICE.get_or_init(|| {
        if command_exists("magick") { "magick" } else { "convert" }
    })
}

/// Extract height from PNG IHDR chunk (bytes 20-23, big-endian u32)
fn png_height(data: &[u8]) -> Option<u32> {
    if data.len() >= 24 && &data[0..4] == b"\x89PNG" {
        Some(u32::from_be_bytes([data[20], data[21], data[22], data[23]]))
    } else {
        None
    }
}

/// Extract width from PNG IHDR chunk (bytes 16-19, big-endian u32)
fn png_width(data: &[u8]) -> Option<u32> {
    if data.len() >= 20 && &data[0..4] == b"\x89PNG" {
        Some(u32::from_be_bytes([data[16], data[17], data[18], data[19]]))
    } else {
        None
    }
}

/// The terminal, in characters: (columns, rows). Falls back to 80×24
/// when the ioctl says nothing.
/// A raw RGBA frame as one kitty transmit-and-place, for a program that
/// redraws whole frames, such as a game: image `id` is replaced on
/// every call, the placement fills `cols` by `rows` cells from the
/// cursor, the cursor stays put and the terminal stays quiet. Send the
/// returned string after moving the cursor to the top-left cell.
pub fn kitty_frame(id: u32, width: u32, height: u32, cols: u16, rows: u16, rgba: &[u8]) -> String {
    if in_glass() {
        if let Some(name) = shm_write(rgba, 8) {
            let payload = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
            return format!("\x1b_Ga=T,f=32,t=s,i={},s={},v={},c={},r={},q=2,C=1;{}\x1b\\",
                id, width, height, cols, rows, payload);
        }
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(rgba);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    let mut out = String::with_capacity(encoded.len() + chunks.len() * 16 + 64);
    for (idx, chunk) in chunks.iter().enumerate() {
        let more = if idx + 1 < chunks.len() { 1 } else { 0 };
        let chunk = std::str::from_utf8(chunk).unwrap_or("");
        if idx == 0 {
            out.push_str(&format!("\x1b_Ga=T,f=32,i={},s={},v={},c={},r={},q=2,C=1,m={};{}\x1b\\",
                id, width, height, cols, rows, more, chunk));
        } else {
            out.push_str(&format!("\x1b_Gm={};{}\x1b\\", more, chunk));
        }
    }
    out
}

/// As [`kitty_frame`], with RGB pixels and no alpha: a quarter less to
/// move, and the format glass draws fastest. Inside glass the pixels go
/// over shared memory; elsewhere as base64 in chunks.
pub fn kitty_frame_rgb(id: u32, width: u32, height: u32, cols: u16, rows: u16, rgb: &[u8]) -> String {
    if in_glass() {
        if let Some(name) = shm_write(rgb, 8) {
            let payload = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
            return format!("\x1b_Ga=T,f=24,t=s,i={},s={},v={},c={},r={},q=2,C=1;{}\x1b\\",
                id, width, height, cols, rows, payload);
        }
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(rgb);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    let mut out = String::with_capacity(encoded.len() + chunks.len() * 16 + 64);
    for (idx, chunk) in chunks.iter().enumerate() {
        let more = if idx + 1 < chunks.len() { 1 } else { 0 };
        let chunk = std::str::from_utf8(chunk).unwrap_or("");
        if idx == 0 {
            out.push_str(&format!("\x1b_Ga=T,f=24,i={},s={},v={},c={},r={},q=2,C=1,m={};{}\x1b\\",
                id, width, height, cols, rows, more, chunk));
        } else {
            out.push_str(&format!("\x1b_Gm={};{}\x1b\\", more, chunk));
        }
    }
    out
}

/// Put a picture on a bare console, sized to the box of cells it was
/// asked for and placed where that box begins.
fn fb_display(image_path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
    let Some(mut screen) = fb::Screen::open() else { return false };
    let Ok(picture) = image::open(image_path) else { return false };
    let (bw, bh) = cell_box(max_width, max_height);
    let fitted = picture.resize(bw as u32, bh as u32, image::imageops::FilterType::Triangle);
    let rgba = fitted.to_rgba8();
    let (px, py) = cell_to_pixel(x, y);
    screen.blit(px, py, rgba.width() as usize, rgba.height() as usize, rgba.as_raw())
}

/// The pixel a cell starts at. Cells count from one, pixels from zero.
fn cell_to_pixel(x: u16, y: u16) -> (i64, i64) {
    let (cw, ch) = get_cell_size();
    ((x.saturating_sub(1) as i64) * cw as i64, (y.saturating_sub(1) as i64) * ch as i64)
}

/// Delete image `id` and its placements.
pub fn kitty_forget(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={},q=2\x1b\\", id)
}

pub fn terminal_size() -> (u16, u16) {
    match crossterm_size() {
        Ok((rows, cols)) => (cols, rows),
        Err(_) => (80, 24),
    }
}

/// The exact size in pixels of a block of `cols` × `rows` cells on this
/// terminal, from the window's pixel size. A cell is often not a whole
/// number of pixels wide, and a canvas built from a rounded cell size is
/// stretched by the terminal to fit the cells; one built from this is not.
pub fn cell_box(cols: u16, rows: u16) -> (usize, usize) {
    if let Ok((trows, tcols)) = crossterm_size() {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) };
        if result == 0 && ws.ws_xpixel > 0 && ws.ws_ypixel > 0 && tcols > 0 && trows > 0 {
            let w = (cols as f64 * ws.ws_xpixel as f64 / tcols as f64).round() as usize;
            let h = (rows as f64 * ws.ws_ypixel as f64 / trows as f64).round() as usize;
            return (w.max(1), h.max(1));
        }
        // The same on a console, where the screen is the only source.
        if let Some((sw, sh)) = fb::screen_size() {
            if tcols > 0 && trows > 0 {
                let w = cols as usize * sw / tcols as usize;
                let h = rows as usize * sh / trows as usize;
                return (w.max(1), h.max(1));
            }
        }
    }
    (cols as usize * 10, rows as usize * 20)
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
        // A bare console reports no pixels at all. The screen itself
        // knows how big it is, and the cells divide it evenly.
        if let Some((w, h)) = fb::screen_size() {
            if cols > 0 && rows > 0 {
                return ((w / cols as usize) as u16, (h / rows as usize) as u16);
            }
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



// ── Canvas: a picture the size of a block of cells ──────────────────────

/// A picture drawn pixel by pixel to fill `cols` × `rows` cells, shown
/// with `Display::show_canvas`. It starts opaque black. Images sit over
/// the text, so a cell that must show text gets a `hole`: transparent
/// pixels that the text shows through.
pub struct Canvas {
    pub cols: u16,
    pub rows: u16,
    /// One cell in whole pixels, rounded. A cell is often not a whole
    /// number of pixels wide (1920 over 181 columns is 10.6), so geometry
    /// should use `cell_w` and `cell_h`, which are exact.
    pub cell: (u16, u16),
    /// Width and height in pixels: whole cells, so the image is placed
    /// without stretching.
    pub w: usize,
    pub h: usize,
    /// RGBA, row by row.
    pub rgba: Vec<u8>,
}

impl Canvas {
    /// A canvas for `cols` × `rows` cells of this terminal: exactly the
    /// pixels that block covers, so the picture lands without stretching.
    pub fn new(cols: u16, rows: u16) -> Canvas {
        let (w, h) = cell_box(cols, rows);
        let cell = (
            (w as f64 / cols.max(1) as f64).round().max(1.0) as u16,
            (h as f64 / rows.max(1) as f64).round().max(1.0) as u16,
        );
        Canvas { cols, rows, cell, w, h, rgba: [0, 0, 0, 255].repeat(w * h) }
    }

    /// The same with a given cell size, for tests or a size known already.
    pub fn with_cell(cols: u16, rows: u16, cell: (u16, u16)) -> Canvas {
        let cell = (cell.0.max(1), cell.1.max(1));
        let (w, h) = (cols as usize * cell.0 as usize, rows as usize * cell.1 as usize);
        Canvas { cols, rows, cell, w, h, rgba: [0, 0, 0, 255].repeat(w * h) }
    }

    /// `new` when `cell` is None, `with_cell` when it is given: one call
    /// for code that runs on the terminal and in tests alike.
    pub fn sized(cols: u16, rows: u16, cell: Option<(u16, u16)>) -> Canvas {
        match cell {
            Some(c) => Canvas::with_cell(cols, rows, c),
            None => Canvas::new(cols, rows),
        }
    }

    /// One cell's width in pixels, exact.
    pub fn cell_w(&self) -> f64 {
        self.w as f64 / self.cols.max(1) as f64
    }

    /// One cell's height in pixels, exact.
    pub fn cell_h(&self) -> f64 {
        self.h as f64 / self.rows.max(1) as f64
    }

    /// Set one pixel, opaque. Off the canvas is ignored.
    pub fn put(&mut self, x: usize, y: usize, rgb: (u8, u8, u8)) {
        if x < self.w && y < self.h {
            let o = (y * self.w + x) * 4;
            self.rgba[o] = rgb.0;
            self.rgba[o + 1] = rgb.1;
            self.rgba[o + 2] = rgb.2;
            self.rgba[o + 3] = 255;
        }
    }

    /// Make `cells` cells from (`row`, `col`) transparent, so text printed
    /// there shows through the picture.
    pub fn hole(&mut self, row: usize, col: usize, cells: usize) {
        let (cw, ch) = (self.cell_w(), self.cell_h());
        let (y0, y1) = ((row as f64 * ch).round() as usize, ((row + 1) as f64 * ch).round() as usize);
        let (x0, x1) = ((col as f64 * cw).round() as usize, ((col + cells) as f64 * cw).round() as usize);
        for y in y0..y1.min(self.h) {
            for x in x0..x1.min(self.w) {
                self.rgba[(y * self.w + x) * 4 + 3] = 0;
            }
        }
    }

    /// Make the whole canvas see-through, for a picture that only paints
    /// what is there and leaves the terminal's background elsewhere.
    pub fn see_through(&mut self) {
        for px in self.rgba.chunks_mut(4) {
            px[3] = 0;
        }
    }

    /// Paint `rgb` over one pixel with cover `a` (0 to 1), source over
    /// what is there. Off the canvas is ignored.
    pub fn blend(&mut self, x: i64, y: i64, rgb: (u8, u8, u8), a: f64) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let o = (y as usize * self.w + x as usize) * 4;
        let a = a.clamp(0.0, 1.0);
        let da = self.rgba[o + 3] as f64 / 255.0;
        let out = a + da * (1.0 - a);
        if out <= 0.0 {
            return;
        }
        for (k, s) in [rgb.0, rgb.1, rgb.2].into_iter().enumerate() {
            let d = self.rgba[o + k] as f64;
            self.rgba[o + k] = ((s as f64 * a + d * da * (1.0 - a)) / out).round().clamp(0.0, 255.0) as u8;
        }
        self.rgba[o + 3] = (out * 255.0).round() as u8;
    }

    /// A soft-edged disc of radius `r` pixels about (`cx`, `cy`).
    pub fn disc(&mut self, cx: f64, cy: f64, r: f64, rgb: (u8, u8, u8), a: f64) {
        for y in (cy - r - 1.0).floor() as i64..=(cy + r + 1.0).ceil() as i64 {
            for x in (cx - r - 1.0).floor() as i64..=(cx + r + 1.0).ceil() as i64 {
                let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
                let k = (r + 0.5 - d).clamp(0.0, 1.0);
                if k > 0.0 {
                    self.blend(x, y, rgb, a * k);
                }
            }
        }
    }

    /// A one-pixel ring of radius `r` about (`cx`, `cy`).
    pub fn ring(&mut self, cx: f64, cy: f64, r: f64, rgb: (u8, u8, u8), a: f64) {
        for y in (cy - r - 1.0).floor() as i64..=(cy + r + 1.0).ceil() as i64 {
            for x in (cx - r - 1.0).floor() as i64..=(cx + r + 1.0).ceil() as i64 {
                let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
                let k = 1.0 - (d - r).abs();
                if k > 0.0 {
                    self.blend(x, y, rgb, a * k);
                }
            }
        }
    }

    /// A line from `a` to `b`, `thick` pixels wide.
    pub fn line(&mut self, a: (f64, f64), b: (f64, f64), thick: f64, rgb: (u8, u8, u8), alpha: f64) {
        let n = ((b.0 - a.0).abs().max((b.1 - a.1).abs()) * 1.5).ceil().max(1.0) as usize;
        let r = (thick / 2.0).max(0.5);
        for i in 0..=n {
            let f = i as f64 / n as f64;
            let (x, y) = (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f);
            if r <= 0.6 {
                self.blend(x as i64, y as i64, rgb, alpha);
            } else {
                self.disc(x, y, r, rgb, alpha);
            }
        }
    }

    /// Settle every half-covered pixel to drawn or not, by an ordered
    /// dither. Glass paints a pixel in full or not at all, so a soft edge
    /// over the terminal's background has to be decided before sending.
    pub fn settle_alpha(&mut self) {
        const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
        for y in 0..self.h {
            for x in 0..self.w {
                let o = (y * self.w + x) * 4;
                let a = self.rgba[o + 3];
                if a == 0 || a == 255 {
                    continue;
                }
                self.rgba[o + 3] = if a > BAYER[y % 4][x % 4] * 16 + 8 { 255 } else { 0 };
            }
        }
    }

    /// The canvas as a PNG, encoded for speed rather than size.
    pub fn png(&self) -> Vec<u8> {
        let mut png = Vec::new();
        let enc = image::codecs::png::PngEncoder::new_with_quality(
            &mut png, image::codecs::png::CompressionType::Fast, image::codecs::png::FilterType::Up);
        let _ = image::ImageEncoder::write_image(enc, &self.rgba, self.w as u32, self.h as u32, image::ExtendedColorType::Rgba8);
        png
    }
}

impl Display {
    /// The console screen, opened the first time it is wanted.
    fn fb_screen(&mut self) -> Option<&mut fb::Screen> {
        if self.fb.is_none() {
            self.fb = fb::Screen::open();
        }
        self.fb.as_mut()
    }

    /// Lay already-made pixels on a bare console at a cell position.
    fn fb_put(&mut self, rgba: &image::RgbaImage, x: u16, y: u16) -> bool {
        let (px, py) = cell_to_pixel(x, y);
        let (w, h) = (rgba.width() as usize, rgba.height() as usize);
        self.fb_shown = Some((px, py, w, h));
        let raw = rgba.as_raw();
        match self.fb_screen() {
            Some(screen) => screen.blit(px, py, w, h, raw),
            None => false,
        }
    }

    /// The same, for a canvas the caller drew.
    fn fb_canvas(&mut self, canvas: &Canvas, x: u16, y: u16) -> bool {
        let (px, py) = cell_to_pixel(x, y);
        self.fb_shown = Some((px, py, canvas.w, canvas.h));
        let (w, h) = (canvas.w, canvas.h);
        match self.fb_screen() {
            Some(screen) => screen.blit(px, py, w, h, &canvas.rgba),
            None => false,
        }
    }

    /// Show `canvas` with its top-left cell at column `x`, row `y`, 1-based.
    pub fn show_canvas(&mut self, canvas: &Canvas, x: u16, y: u16) -> bool {
        // On a console the pixels are already in the shape the screen
        // wants, so they go straight there: no PNG made, none decoded.
        if self.protocol == Some(Protocol::Framebuffer) {
            return self.fb_canvas(canvas, x, y);
        }
        self.show_png(&canvas.png(), x, y, canvas.cols, canvas.rows)
    }

    /// Show `canvas` in place of whatever this display showed before. The
    /// new picture goes up first and the old ones come down after it, so
    /// a redraw never shows a moment without a picture, which on a
    /// running animation reads as flicker.
    pub fn swap_canvas(&mut self, canvas: &Canvas, x: u16, y: u16) -> bool {
        let old: Vec<u32> = std::mem::take(&mut self.active_ids);
        let ok = self.show_canvas(canvas, x, y);
        if matches!(self.protocol, Some(Protocol::Kitty)) && !old.is_empty() {
            // Uppercase I: the placements and the image data go, so an
            // animation does not fill the terminal with old frames.
            for id in &old {
                print!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", id);
            }
            io::stdout().flush().ok();
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn holes_follow_exact_cells_on_an_uneven_canvas() {
        // 106 pixels over 10 columns: a cell is 10.6 wide.
        let mut c = Canvas { cols: 10, rows: 2, cell: (11, 20), w: 106, h: 40, rgba: [0, 0, 0, 255].repeat(106 * 40) };
        assert!((c.cell_w() - 10.6).abs() < 1e-9);
        c.hole(1, 9, 1);
        let clear: Vec<usize> = (0..106).filter(|&x| c.rgba[(30 * 106 + x) * 4 + 3] == 0).collect();
        assert_eq!((clear[0], *clear.last().unwrap()), (95, 105), "the last cell runs from 95 to 105");
        assert_eq!(c.rgba[(10 * 106 + 100) * 4 + 3], 255, "the row above is untouched");
    }

    #[test]
    fn drawing_blends_and_settles_to_drawn_or_not() {
        let mut c = Canvas::with_cell(4, 2, (10, 20));
        c.see_through();
        assert!(c.rgba.chunks(4).all(|p| p[3] == 0));
        c.disc(20.0, 20.0, 5.0, (200, 100, 0), 1.0);
        let o = (20 * 40 + 20) * 4;
        assert_eq!(&c.rgba[o..o + 4], &[200, 100, 0, 255], "the middle is solid");
        let soft = c.rgba.chunks(4).filter(|p| p[3] > 0 && p[3] < 255).count();
        assert!(soft > 0, "the edge is soft before settling");
        c.line((0.0, 0.0), (39.0, 39.0), 1.0, (0, 0, 255), 0.5);
        c.settle_alpha();
        assert!(c.rgba.chunks(4).all(|p| p[3] == 0 || p[3] == 255), "settled");
        assert!(c.rgba.chunks(4).filter(|p| p[3] == 255).count() > 60);
    }

    #[test]
    fn a_canvas_is_whole_cells_with_holes_the_text_shows_through() {
        let mut c = Canvas::with_cell(4, 2, (10, 20));
        assert_eq!((c.w, c.h), (40, 40));
        c.put(5, 5, (1, 2, 3));
        c.hole(1, 2, 1);
        let clear = c.rgba.chunks(4).filter(|p| p[3] == 0).count();
        assert_eq!(clear, 200, "one cell of 10 by 20 is transparent");
        assert_eq!(c.rgba[(25 * 40 + 25) * 4 + 3], 0);
        assert_eq!(c.rgba[(25 * 40 + 15) * 4 + 3], 255);
        let img = image::load_from_memory(&c.png()).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (40, 40));
        assert_eq!(img.get_pixel(5, 5).0, [1, 2, 3, 255]);
        assert_eq!(img.get_pixel(25, 25).0[3], 0);
    }

    #[test]
    fn a_png_from_memory_is_placed_and_cleared() {
        let mut png = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut png),
            &[128u8; 20 * 40], 20, 40, image::ExtendedColorType::L8,
        ).unwrap();
        let mut d = super::Display::with_mode("kitty");
        assert!(d.show_png(&png, 1, 1, 2, 2));
        assert_eq!(d.active_ids.len(), 1);
        d.clear(1, 1, 2, 2, 80, 24);
        assert!(d.active_ids.is_empty());
        assert!(!super::Display::with_mode("off").show_png(&png, 1, 1, 2, 2));
    }

    use super::*;

    /// The two tables have to be each other's inverse, or every colour
    /// drifts a little every time an image is scaled.
    #[test]
    fn the_light_tables_round_trip() {
        let lin = srgb_to_linear();
        for v in 0u8..=255 {
            let back = linear_to_srgb(lin[v as usize]);
            assert!(back.abs_diff(v) <= 1, "{v} came back as {back}");
        }
        assert_eq!(lin[0], 0);
        assert_eq!(lin[255], 65535);
        // Middle grey is dark in linear light: that is the whole point.
        assert!(lin[128] < 22000, "sRGB 128 is {} of 65535", lin[128]);
    }

    /// Shrinking a black-and-white checkerboard has to give the grey it
    /// physically is. Adding sRGB bytes would give 128, which is the
    /// classic too-dark thumbnail; in linear light it comes out at 188.
    #[test]
    fn shrinking_does_not_darken() {
        let mut px = Vec::new();
        for y in 0..2u32 {
            for x in 0..2u32 {
                let v = if (x + y) % 2 == 0 { 255u8 } else { 0u8 };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let out = box_average(&px, 2, 2, 1, 1);
        assert!(
            (out[0] as i32 - 188).abs() <= 2,
            "half black half white came out at {}, not 188",
            out[0]
        );
        assert_eq!(out[3], 255);
    }

    /// A transparent pixel must not tint the ones it is averaged with,
    /// and a fully transparent block stays transparent.
    #[test]
    fn transparency_does_not_bleed() {
        let px = [
            255, 0, 0, 255, // red, opaque
            0, 255, 0, 0,   // green, invisible
        ];
        let out = box_average(&px, 2, 1, 1, 1);
        assert_eq!((out[0], out[1], out[2]), (255, 0, 0));
        assert_eq!(out[3], 127);
        let clear = box_average(&[9, 9, 9, 0, 9, 9, 9, 0], 2, 1, 1, 1);
        assert_eq!(clear[3], 0);
    }

    #[test]
    fn fitting_keeps_the_shape() {
        assert_eq!(fit_within(1000, 500, 100, 100), (100, 50));
        assert_eq!(fit_within(500, 1000, 100, 100), (50, 100));
        assert_eq!(fit_within(10, 10, 100, 40), (40, 40));
        // Never zero, however extreme the shape.
        let (w, h) = fit_within(4000, 3, 80, 24);
        assert!(w >= 1 && h >= 1);
    }

    /// A flat grey fills about half the dots in a cell, which is what a
    /// fixed threshold cannot do: it would give none or all.
    #[test]
    fn the_dither_keeps_the_midtones() {
        let flat = |v: u8| vec![v, v, v, 255].repeat(8);
        let lit = |v: u8| braille_cell(2, 4, &flat(v), 0, 0).0.count_ones();
        assert_eq!(lit(0), 0, "black should light nothing");
        assert_eq!(lit(255), 8, "white should light everything");
        assert_eq!(lit(128), 4, "mid grey should light half");
        // And it climbs all the way up, one dot at a time.
        let steps: Vec<u32> = (0..=8).map(|i| lit((i * 255 / 8) as u8)).collect();
        assert_eq!(steps, vec![0, 1, 2, 3, 4, 5, 6, 7, 8], "{steps:?}");
    }

    /// A cell shows the colour of the dots that lit, not of the whole
    /// neighbourhood: a red dot on black stays red.
    #[test]
    fn a_cell_takes_the_colour_of_its_ink() {
        let mut px = vec![0u8; 2 * 4 * 4];
        px[0..4].copy_from_slice(&[255, 40, 40, 255]); // one bright red dot
        let (mask, colour) = braille_cell(2, 4, &px, 0, 0);
        assert_eq!(mask, 0x01);
        assert_eq!(colour, Some((255, 40, 40)));
    }

    /// Half blocks: two colours per cell, and runs of one colour emit
    /// the escape once.
    #[test]
    fn half_blocks_carry_two_colours() {
        // Two cells wide, two pixels tall: red over blue, twice.
        let mut px = Vec::new();
        for _ in 0..2 {
            px.extend_from_slice(&[255, 0, 0, 255]);
        }
        for _ in 0..2 {
            px.extend_from_slice(&[0, 0, 255, 255]);
        }
        let frame = half_block_frame(2, 2, &px, 1, 1);
        assert!(frame.contains("38;2;255;0;0"), "no red foreground: {frame:?}");
        assert!(frame.contains("48;2;0;0;255"), "no blue background: {frame:?}");
        assert_eq!(frame.matches('▀').count(), 2);
        // One SGR pair for the run, not one per cell.
        assert_eq!(frame.matches("38;2;").count(), 1, "{frame:?}");
    }

    /// A transparent half draws the other half as a glyph, and a fully
    /// transparent cell leaves the terminal alone.
    #[test]
    fn half_blocks_respect_transparency() {
        let px = [
            0, 0, 0, 0,       // top: clear
            10, 200, 10, 255, // bottom: green
            0, 0, 0, 0,       // top: clear
            0, 0, 0, 0,       // bottom: clear
        ];
        // Two columns, two rows: column 0 is half green, column 1 empty.
        let mut grid = vec![0u8; 2 * 2 * 4];
        grid[0..4].copy_from_slice(&px[0..4]);
        grid[4..8].copy_from_slice(&px[8..12]);
        grid[8..12].copy_from_slice(&px[4..8]);
        grid[12..16].copy_from_slice(&px[12..16]);
        let frame = half_block_frame(2, 2, &grid, 1, 1);
        assert!(frame.contains('▄'), "lower half should be drawn: {frame:?}");
        assert!(frame.ends_with("\x1b[0m") || frame.contains(' '));
        assert!(!frame.contains("48;2;"), "nothing to paint behind: {frame:?}");
    }
}

#[cfg(test)]
mod shm_tests {
    use super::*;

    #[test]
    fn a_png_goes_to_shared_memory_as_raw_pixels_with_the_right_header() {
        let mut png = Vec::new();
        let px = image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255]).unwrap();
        px.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let seq = kitty_transmit_shm(7, &png).expect("decodes");
        assert!(seq.starts_with("\x1b_Ga=t,f=32,t=s,i=7,s=2,v=1,q=2;"), "{}", seq);
        let payload = seq.trim_end_matches("\x1b\\").rsplit(';').next().unwrap();
        let name = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(payload).unwrap()).unwrap();
        let bytes = std::fs::read(format!("/dev/shm/{}", name)).unwrap();
        assert_eq!(bytes, vec![255, 0, 0, 255, 0, 255, 0, 255]);
        let _ = std::fs::remove_file(format!("/dev/shm/{}", name));
    }
}
