//! Real pixels on a bare console.
//!
//! A terminal under X speaks kitty or sixel. The Linux console speaks
//! neither, but it has the screen itself: `/dev/fb0`, the pixels of the
//! display laid out row after row. Anyone in the `video` group may write
//! there, and what they write appears at once.
//!
//! The console goes on drawing its text as it always does, over these
//! pixels. So an app leaves a hole where the picture goes, exactly as it
//! does under kitty, and the two live side by side.
//!
//! Nothing here runs unless the screen is a console with no X on it.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Ask the driver how the screen is laid out.
const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;

/// Where one colour sits inside a pixel, in bits.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Bitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

/// The kernel's `fb_var_screeninfo`, field for field.
#[repr(C)]
struct VarInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: Bitfield,
    green: Bitfield,
    blue: Bitfield,
    transp: Bitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

/// The console screen, open and ready to be drawn on.
pub struct Screen {
    file: File,
    /// The size of the screen in pixels.
    pub w: usize,
    pub h: usize,
    /// Bytes from the start of one row to the start of the next.
    stride: usize,
    /// Bytes per pixel, and which byte holds which colour.
    bytes: usize,
    red: usize,
    green: usize,
    blue: usize,
}

impl Screen {
    /// Open the screen, or give back nothing when there is none to open.
    pub fn open() -> Option<Screen> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/fb0").ok()?;
        let mut var: VarInfo = unsafe { std::mem::zeroed() };
        let asked = unsafe {
            libc::ioctl(std::os::unix::io::AsRawFd::as_raw_fd(&file), FBIOGET_VSCREENINFO, &mut var)
        };
        if asked != 0 {
            return None;
        }
        // Only the plain 32-bit screens, which is every laptop and every
        // desktop this century. Anything else keeps the text fallback.
        if var.bits_per_pixel != 32 {
            return None;
        }
        let stride = read_number("stride").unwrap_or(var.xres_virtual as usize * 4);
        Some(Screen {
            file,
            w: var.xres as usize,
            h: var.yres as usize,
            stride,
            bytes: 4,
            red: (var.red.offset / 8) as usize,
            green: (var.green.offset / 8) as usize,
            blue: (var.blue.offset / 8) as usize,
        })
    }

    /// Lay a picture on the screen at a pixel position.
    ///
    /// `rgba` is four bytes a pixel. A pixel with nothing in its alpha
    /// is left as it was, which is how a canvas leaves holes for text.
    pub fn blit(&self, x: i64, y: i64, w: usize, h: usize, rgba: &[u8]) -> bool {
        if w == 0 || h == 0 || rgba.len() < w * h * 4 {
            return false;
        }
        let mut row = vec![0u8; self.stride];
        for line in 0..h {
            let sy = y + line as i64;
            if sy < 0 || sy as usize >= self.h {
                continue;
            }
            // Where this row starts and ends on the screen, cut to fit.
            let from = x.max(0) as usize;
            let to = ((x + w as i64).max(0) as usize).min(self.w);
            if from >= to {
                continue;
            }
            let bytes = (to - from) * self.bytes;
            let at = sy as usize * self.stride + from * self.bytes;
            // Read what is there, so a see-through pixel keeps it.
            if self.file.read_exact_at(&mut row[..bytes], at as u64).is_err() {
                continue;
            }
            for (i, sx) in (from..to).enumerate() {
                let src = (line * w + (sx as i64 - x) as usize) * 4;
                if rgba[src + 3] == 0 {
                    continue;
                }
                let px = i * self.bytes;
                row[px + self.red] = rgba[src];
                row[px + self.green] = rgba[src + 1];
                row[px + self.blue] = rgba[src + 2];
            }
            if self.file.write_all_at(&row[..bytes], at as u64).is_err() {
                return false;
            }
        }
        true
    }

    /// Paint a block of the screen one colour, for taking a picture away.
    pub fn fill(&self, x: i64, y: i64, w: usize, h: usize, rgb: (u8, u8, u8)) -> bool {
        let mut one = vec![0u8; w.min(self.w) * self.bytes];
        for px in one.chunks_exact_mut(self.bytes) {
            px[self.red] = rgb.0;
            px[self.green] = rgb.1;
            px[self.blue] = rgb.2;
        }
        for line in 0..h {
            let sy = y + line as i64;
            if sy < 0 || sy as usize >= self.h {
                continue;
            }
            let from = x.max(0) as usize;
            let to = ((x + w as i64).max(0) as usize).min(self.w);
            if from >= to {
                continue;
            }
            let bytes = (to - from) * self.bytes;
            let at = sy as usize * self.stride + from * self.bytes;
            if self.file.write_all_at(&one[..bytes.min(one.len())], at as u64).is_err() {
                return false;
            }
        }
        true
    }
}

/// One number out of `/sys/class/graphics/fb0/`.
fn read_number(what: &str) -> Option<usize> {
    std::fs::read_to_string(format!("/sys/class/graphics/fb0/{what}"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Is this a console whose screen we may draw on?
///
/// Writing pixels while X owns the screen would fight with it, so a live
/// display rules this out. So does a framebuffer we cannot write to.
pub fn there() -> bool {
    if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return false;
    }
    if !Path::new("/dev/fb0").exists() {
        return false;
    }
    OpenOptions::new().read(true).write(true).open("/dev/fb0").is_ok()
}

#[cfg(test)]
impl Screen {
    /// A screen made of a plain file, for the tests: the same maths,
    /// nothing wired to a display.
    fn fake(file: File, w: usize, h: usize) -> Screen {
        Screen { file, w, h, stride: w * 4, bytes: 4, red: 2, green: 1, blue: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A blank screen of `w` by `h`, as a file we can read back.
    fn screen(w: usize, h: usize) -> (Screen, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("glow-fb-test-{}-{:?}.raw", std::process::id(), std::thread::current().id()));
        let mut f = File::create(&path).unwrap();
        f.write_all(&vec![7u8; w * h * 4]).unwrap();
        let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        (Screen::fake(file, w, h), path)
    }

    fn pixel(path: &std::path::Path, w: usize, x: usize, y: usize) -> [u8; 4] {
        let bytes = std::fs::read(path).unwrap();
        let at = (y * w + x) * 4;
        [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]
    }

    #[test]
    fn the_layout_matches_what_the_kernel_hands_back() {
        // The ioctl fills this struct; a wrong size means wrong answers.
        assert_eq!(std::mem::size_of::<Bitfield>(), 12);
        assert_eq!(std::mem::size_of::<VarInfo>(), 160);
    }

    #[test]
    fn a_picture_lands_where_it_was_put_in_the_screen_s_own_byte_order() {
        let (s, path) = screen(8, 4);
        // One red pixel, at the second column of the second row.
        let red = vec![220u8, 30, 10, 255];
        assert!(s.blit(1, 1, 1, 1, &red));
        assert_eq!(pixel(&path, 8, 1, 1), [10, 30, 220, 7], "blue, green, red, and the last byte left alone");
        assert_eq!(pixel(&path, 8, 0, 0), [7, 7, 7, 7], "the pixel beside it is untouched");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_see_through_pixel_keeps_what_was_under_it() {
        let (s, path) = screen(4, 2);
        let two = vec![1u8, 2, 3, 0, 9, 8, 7, 255];
        assert!(s.blit(0, 0, 2, 1, &two));
        assert_eq!(pixel(&path, 4, 0, 0), [7, 7, 7, 7], "nothing in its alpha, so nothing written");
        assert_eq!(pixel(&path, 4, 1, 0), [7, 8, 9, 7], "the solid one went in");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_picture_hanging_over_the_edge_is_cut_to_fit() {
        let (s, path) = screen(4, 2);
        let row = vec![100u8, 100, 100, 255].repeat(4);
        // Two of the four pixels are past the right edge.
        assert!(s.blit(2, 0, 4, 1, &row));
        assert_eq!(pixel(&path, 4, 3, 0), [100, 100, 100, 7], "the last pixel on the screen");
        assert_eq!(pixel(&path, 4, 0, 1), [7, 7, 7, 7], "nothing wrapped onto the next row");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn taking_a_picture_away_paints_the_block_out() {
        let (s, path) = screen(4, 2);
        assert!(s.fill(0, 0, 2, 2, (0, 0, 0)));
        assert_eq!(pixel(&path, 4, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&path, 4, 2, 0), [7, 7, 7, 7], "only the block asked for");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_display_rules_out_the_console() {
        if std::env::var_os("DISPLAY").is_some() {
            assert!(!there(), "X owns the screen, so the framebuffer is not ours");
        }
    }
}
