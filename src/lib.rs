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
    /// Universal text fallback: render the image into Unicode braille
    /// glyphs (`U+2800`–`U+28FF`). Each cell holds a 2×4 dot grid, so a
    /// W×H char block yields a (2W)×(4H) "pixel" image. No external
    /// dependency beyond `convert`. Works over SSH, in tmux without
    /// passthrough, and on every terminal that can render Unicode.
    Braille,
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

pub struct Display {
    protocol: Option<Protocol>,
    active_ids: Vec<u32>,
    image_cache: HashMap<String, (u32, u16, u16)>,  // (image_id, natural_pixel_w, natural_pixel_h)
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

    /// Force a specific display mode ("auto", "ascii", "off", "kitty",
    /// "sixel", "braille"). "braille" needs `convert` only — useful as a
    /// graphics-free preview over SSH or in tmux without passthrough.
    pub fn with_mode(mode: &str) -> Self {
        let protocol = match mode {
            "ascii" | "chafa" => {
                if command_exists("chafa") { Some(Protocol::Chafa) } else { None }
            }
            "kitty" => Some(Protocol::Kitty),
            "sixel" => Some(Protocol::Sixel),
            "braille" => {
                if command_exists(imagemagick_cmd()) { Some(Protocol::Braille) } else { None }
            }
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
            Protocol::Braille => braille_display(image_path, x, y, max_width, max_height),
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
            Some(Protocol::Braille) => {
                // Braille is text-based, cleared by terminal redraw
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
        let cache_key = format!("{}:{}x{}:{}", image_path, pixel_w, pixel_h, mtime);
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
        let id = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() % 4294967295) as u32;
        let id = if id == 0 { 1 } else { id };
        // Two-tier lookup: RAM, then on-disk PNG cache. Falls through
        // to `convert` only when neither tier has the resized PNG.
        let png_data = match cache_get(&self.png_cache, &cache_key) {
            Some(data) => data,
            None => {
                let output = Command::new(imagemagick_cmd())
                    .arg(format!("{}[0]", image_path))
                    .arg("-auto-orient")
                    .arg("-resize")
                    .arg(format!("{}x{}>", pixel_w, pixel_h))
                    .arg("PNG:-")
                    .output();
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

        let pixel_w = max_width as u32 * cell_w as u32;
        let pixel_h = max_height as u32 * cell_h as u32;

        // Cache by path + width + height + mtime. Including height matters
        // because a tall image may have to be height-clamped on a short
        // pane and width-clamped on a wide pane — different convert outputs.
        let mtime = std::fs::metadata(image_path)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0);
        let cache_key = format!("{}:{}x{}:{}", image_path, pixel_w, pixel_h, mtime);

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
            // Generate new ID
            let id = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() % 4294967295) as u32;
            let id = if id == 0 { 1 } else { id };

            // Two-tier cache: in-RAM first, fall back to the on-disk
            // PNG cache populated by previous runs. Either path skips
            // the `convert` subprocess entirely. The disk fallback is
            // what makes the SECOND launch of kastrup show images
            // instantly — RAM is empty on every new process.
            let png_data = match cache_get(&self.png_cache, &cache_key) {
                Some(data) => data,
                None => {
                    // Fallback: convert synchronously
                    // Resize to fit BOTH max width AND max height (the `>`
                    // suffix means "only shrink, never enlarge"). Without
                    // the height bound, a square image scaled to fit a wide
                    // pane's width can still overflow vertically into the
                    // status bar / next pane.
                    let output = Command::new(imagemagick_cmd())
                        .arg(format!("{}[0]", image_path))
                        .arg("-auto-orient")
                        .arg("-resize")
                        .arg(format!("{}x{}>", pixel_w, pixel_h))
                        .arg("PNG:-")
                        .output();
                    let data = match output {
                        Ok(o) if !o.stdout.is_empty() => o.stdout,
                        _ => return false,
                    };
                    // Populate BOTH tiers so re-shows of the same image
                    // (even across kastrup restarts) skip the convert
                    // subprocess.
                    cache_put(&self.png_cache, cache_key.clone(), data.clone());
                    data
                }
            };

            // Get actual image dimensions from PNG header
            let raw_h = png_height(&png_data).unwrap_or(pixel_w);
            let raw_w = png_width(&png_data).unwrap_or(pixel_w);

            // Pad the PNG up to a cell-aligned multiple of (cell_w, cell_h)
            // with transparent pixels in NorthWest gravity. This is what
            // lets us specify c=N,r=M placement without kitty stretching
            // the image to fill those cells: the padded canvas is exactly
            // c*cell_w by r*cell_h pixels, image content occupies the
            // top-left, the rest is transparent (showing the pane bg).
            // Without this, an image whose height isn't a multiple of
            // cell_h (e.g. 12 px tall vs 24 px cell) gets vertically
            // stretched to fill an integer number of cell rows.
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
                    Err(_) => return false,
                };
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&png_data);
                }
                match child.wait_with_output() {
                    Ok(o) if !o.stdout.is_empty() => o.stdout,
                    _ => return false,
                }
            };

            // Cache the cell-aligned PNG (whether we just padded or
            // it was already aligned). On revisit, the cached entry
            // is ready to transmit: raw_w/raw_h reads from this PNG
            // equal pad_w/pad_h, so the pad subprocess branch above
            // is skipped. That's the difference between a snappy
            // revisit and a slow one — and it also removes the most
            // common silent-failure surface (a 2nd fork of `convert`
            // for padding while the bg preconvert thread is forking
            // too). Replaces the unpadded copy that the convert
            // branch may have inserted earlier under the same key.
            if let Ok(mut c) = self.png_cache.lock() {
                c.insert(cache_key.clone(), png_data.clone());
            }

            let img_pixel_w = pad_w;
            let img_pixel_h = pad_h;

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
        print!("\x1b_Ga=p,i={},c={},r={},z=1,q=2,C=1\x1b\\",
            image_id, cols, rows);
        io::stdout().flush().ok();

        if !already_placed {
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
    Command::new(imagemagick_cmd())
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

    // Braille fallback — last resort, always works as long as `convert` is
    // available. Pure-text output, so survives SSH, tmux without
    // passthrough, weird terminals, etc.
    if command_exists(imagemagick_cmd()) {
        return Some(Protocol::Braille);
    }

    None
}

/// Render `path` into Unicode braille glyphs at (x, y), fitting within
/// `max_width` × `max_height` character cells. Each cell is 2×4 dots, so the
/// effective pixel resolution is (2·max_width) × (4·max_height). Each glyph
/// gets the average color of its 8 source pixels via SGR truecolor — works
/// in every modern terminal that handles 24-bit color.
///
/// Pipeline: `convert` resizes the image (preserving aspect, snapping the
/// fit dims down to multiples of 2×4 so the dot grid tiles cleanly), then
/// dumps raw RGBA. We walk 2×4 blocks: each pixel above the brightness
/// threshold sets one dot, the cell color is the mean of the lit pixels.
fn braille_display(path: &str, x: u16, y: u16, max_width: u16, max_height: u16) -> bool {
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

    let target_px_w = (max_width as u32) * 2;
    let target_px_h = (max_height as u32) * 4;
    let scale = (target_px_w as f64 / orig_w as f64)
        .min(target_px_h as f64 / orig_h as f64);
    let mut fit_w = (orig_w as f64 * scale).round().max(2.0) as u32;
    let mut fit_h = (orig_h as f64 * scale).round().max(4.0) as u32;
    // Snap down to multiples of 2×4 so the braille grid tiles cleanly.
    fit_w -= fit_w % 2;
    fit_h -= fit_h % 4;
    if fit_w < 2 || fit_h < 4 { return false; }

    let raw = Command::new(imagemagick_cmd())
        .arg(format!("{}[0]", path))
        .arg("-auto-orient")
        .arg("-resize").arg(format!("{}x{}!", fit_w, fit_h))
        .arg("-depth").arg("8")
        .arg("RGBA:-")
        .output();
    let bytes = match raw {
        Ok(o) if o.stdout.len() == (fit_w as usize) * (fit_h as usize) * 4 => o.stdout,
        _ => return false,
    };

    let cells_w = (fit_w / 2) as u16;
    let cells_h = (fit_h / 4) as u16;
    // Braille dot bit values — column-major, top to bottom:
    //   col 0 row 0 = 0x01   col 1 row 0 = 0x08
    //   col 0 row 1 = 0x02   col 1 row 1 = 0x10
    //   col 0 row 2 = 0x04   col 1 row 2 = 0x20
    //   col 0 row 3 = 0x40   col 1 row 3 = 0x80
    const DOT_BITS: [(u32, u32, u32); 8] = [
        (0, 0, 0x01), (0, 1, 0x02), (0, 2, 0x04), (0, 3, 0x40),
        (1, 0, 0x08), (1, 1, 0x10), (1, 2, 0x20), (1, 3, 0x80),
    ];

    let mut out = String::with_capacity((cells_w as usize) * (cells_h as usize) * 24);
    for cy in 0..cells_h {
        out.push_str(&format!("\x1b[{};{}H", y + cy, x));
        for cx in 0..cells_w {
            let bx = (cx as u32) * 2;
            let by = (cy as u32) * 4;
            let mut mask: u32 = 0x2800;
            let mut r_sum: u32 = 0;
            let mut g_sum: u32 = 0;
            let mut b_sum: u32 = 0;
            let mut lit: u32 = 0;
            for (dx, dy, bit) in &DOT_BITS {
                let i = (((by + dy) * fit_w + (bx + dx)) * 4) as usize;
                let r = bytes[i] as u32;
                let g = bytes[i + 1] as u32;
                let b = bytes[i + 2] as u32;
                let a = bytes[i + 3] as u32;
                // Lit if not transparent and darker than mid-gray (so light
                // bgs render as empty cells rather than a solid block).
                let bright = (r + g + b) / 3;
                if a > 64 && bright < 200 {
                    mask |= bit;
                    r_sum += r; g_sum += g; b_sum += b;
                    lit += 1;
                }
            }
            let ch = char::from_u32(mask).unwrap_or(' ');
            if lit > 0 {
                let r = (r_sum / lit) as u8;
                let g = (g_sum / lit) as u8;
                let b = (b_sum / lit) as u8;
                out.push_str(&format!("\x1b[38;2;{};{};{}m{}\x1b[39m", r, g, b, ch));
            } else {
                out.push(' ');
            }
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
