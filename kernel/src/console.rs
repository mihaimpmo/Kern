//! Kernel console: text on the framebuffer plus a copy on the serial port.
//! Understands a small subset of ANSI escapes (colours, clear screen,
//! cursor home, clear line) so user programs can use them.

use crate::boot::Framebuffer;
use crate::drivers::serial;
use crate::sync::SpinLock;
use alloc::vec;
use core::fmt;
use font8x8::legacy::BASIC_LEGACY;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 8;
const CELL_H: usize = 10; // 2 pixel rows of spacing, also used for the cursor

const PALETTE: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xcccccc, // normal
    0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff, // bright
];
const DEFAULT_FG: usize = 7;
const DEFAULT_BG: u32 = 0x101014;

enum Escape {
    None,
    Esc,
    Csi { params: [u16; 4], count: usize },
}

struct Screen {
    fb: *mut u8,
    shadow: *mut u8, // copy in RAM so scrolling never reads slow video memory
    width: usize,
    height: usize,
    pitch: usize,
    scale: usize,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    fg: u32,
    bg: u32,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
    escape: Escape,
}

unsafe impl Send for Screen {}

pub struct Console {
    screen: Option<Screen>,
}

static CONSOLE: SpinLock<Console> = SpinLock::new(Console { screen: None });

impl Screen {
    fn color(&self, rgb: u32) -> u32 {
        let (r, g, b) = ((rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff);
        (r << self.red_shift) | (g << self.green_shift) | (b << self.blue_shift)
    }

    fn cell_w(&self) -> usize {
        GLYPH_W * self.scale
    }
    fn cell_h(&self) -> usize {
        CELL_H * self.scale
    }

    /// Write one pixel row segment to the framebuffer (and shadow).
    fn put_row(&mut self, x: usize, y: usize, pixels: &[u32]) {
        let offset = y * self.pitch + x * 4;
        unsafe {
            let src = pixels.as_ptr() as *const u8;
            core::ptr::copy_nonoverlapping(src, self.fb.add(offset), pixels.len() * 4);
            if !self.shadow.is_null() {
                core::ptr::copy_nonoverlapping(src, self.shadow.add(offset), pixels.len() * 4);
            }
        }
    }

    fn draw_cell(&mut self, col: usize, row: usize, ch: u8) {
        let glyph = BASIC_LEGACY[if ch < 128 { ch as usize } else { b'?' as usize }];
        let (fg, bg) = (self.color(self.fg), self.color(self.bg));
        let (x0, y0) = (col * self.cell_w(), row * self.cell_h());
        let mut line = [0u32; GLYPH_W * 4];
        let w = self.cell_w();
        for y in 0..self.cell_h() {
            let gy = y / self.scale;
            let bits = if gy < GLYPH_H { glyph[gy] } else { 0 };
            for (x, px) in line[..w].iter_mut().enumerate() {
                *px = if bits >> (x / self.scale) & 1 != 0 { fg } else { bg };
            }
            self.put_row(x0, y0 + y, &line[..w]);
        }
    }

    /// The cursor is an underline drawn in the spacing rows below the glyph.
    fn draw_cursor(&mut self, visible: bool) {
        if self.col >= self.cols {
            return;
        }
        let color = if visible { self.color(self.fg) } else { self.color(self.bg) };
        let line = [color; GLYPH_W * 4];
        let (x0, y0) = (self.col * self.cell_w(), self.row * self.cell_h() + GLYPH_H * self.scale);
        let w = self.cell_w();
        for y in 0..(CELL_H - GLYPH_H) * self.scale {
            self.put_row(x0, y0 + y, &line[..w]);
        }
    }

    fn clear_rect(&mut self, y0: usize, y1: usize) {
        let bg = self.color(self.bg);
        let line = [bg; 64];
        for y in y0..y1 {
            let mut x = 0;
            while x < self.width {
                let n = (self.width - x).min(64);
                self.put_row(x, y, &line[..n]);
                x += n;
            }
        }
    }

    fn clear(&mut self) {
        self.clear_rect(0, self.height);
        self.col = 0;
        self.row = 0;
    }

    fn scroll(&mut self) {
        let line_bytes = self.cell_h() * self.pitch;
        let total = self.rows * line_bytes;
        unsafe {
            if self.shadow.is_null() {
                core::ptr::copy(self.fb.add(line_bytes), self.fb, total - line_bytes);
            } else {
                core::ptr::copy(self.shadow.add(line_bytes), self.shadow, total - line_bytes);
                core::ptr::copy_nonoverlapping(self.shadow, self.fb, total - line_bytes);
            }
        }
        let y = (self.rows - 1) * self.cell_h();
        self.clear_rect(y, y + self.cell_h());
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= self.rows {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    fn put_byte(&mut self, b: u8) {
        match core::mem::replace(&mut self.escape, Escape::None) {
            Escape::Esc => {
                if b == b'[' {
                    self.escape = Escape::Csi { params: [0; 4], count: 0 };
                }
                return;
            }
            Escape::Csi { mut params, mut count } => {
                match b {
                    b'0'..=b'9' => {
                        let i = count.min(3);
                        params[i] = params[i].saturating_mul(10).saturating_add((b - b'0') as u16);
                        self.escape = Escape::Csi { params, count };
                    }
                    b';' => {
                        count += 1;
                        self.escape = Escape::Csi { params, count };
                    }
                    _ => self.csi(b, &params[..=count.min(3)]),
                }
                return;
            }
            Escape::None => {}
        }
        match b {
            0x1b => self.escape = Escape::Esc,
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => {
                for _ in 0..(4 - self.col % 4) {
                    self.put_byte(b' ');
                }
            }
            0x08 => {
                if self.col > 0 {
                    self.col -= 1;
                    self.draw_cell(self.col, self.row, b' ');
                }
            }
            _ => {
                if self.col >= self.cols {
                    self.newline();
                }
                self.draw_cell(self.col, self.row, b);
                self.col += 1;
            }
        }
    }

    fn csi(&mut self, cmd: u8, params: &[u16]) {
        match cmd {
            b'm' => {
                for &p in params {
                    match p {
                        0 => {
                            self.fg = PALETTE[DEFAULT_FG];
                            self.bg = DEFAULT_BG;
                        }
                        1 => {} // bold: ignored
                        30..=37 => self.fg = PALETTE[(p - 30) as usize],
                        90..=97 => self.fg = PALETTE[(p - 90 + 8) as usize],
                        40..=47 => self.bg = PALETTE[(p - 40) as usize],
                        39 => self.fg = PALETTE[DEFAULT_FG],
                        49 => self.bg = DEFAULT_BG,
                        _ => {}
                    }
                }
            }
            b'J' if params[0] == 2 => self.clear(),
            b'H' => {
                self.row = (params[0].max(1) as usize - 1).min(self.rows - 1);
                self.col = (params.get(1).copied().unwrap_or(1).max(1) as usize - 1).min(self.cols - 1);
            }
            b'K' => {
                for c in self.col..self.cols {
                    self.draw_cell(c, self.row, b' ');
                }
            }
            _ => {}
        }
    }
}

/// Set up the framebuffer console (works before the heap exists).
pub fn init(fb: Option<&Framebuffer>) {
    let Some(fb) = fb else { return };
    if fb.bpp != 32 || fb.address.is_null() {
        return;
    }
    let (width, height) = (fb.width as usize, fb.height as usize);
    let scale = if width >= 2560 { 3 } else if width >= 1600 { 2 } else { 1 };
    let mut screen = Screen {
        fb: fb.address,
        shadow: core::ptr::null_mut(),
        width,
        height,
        pitch: fb.pitch as usize,
        scale,
        cols: width / (GLYPH_W * scale),
        rows: height / (CELL_H * scale),
        col: 0,
        row: 0,
        fg: PALETTE[DEFAULT_FG],
        bg: DEFAULT_BG,
        red_shift: fb.red_mask_shift,
        green_shift: fb.green_mask_shift,
        blue_shift: fb.blue_mask_shift,
        escape: Escape::None,
    };
    screen.clear();
    CONSOLE.lock().screen = Some(screen);
}

/// Once the heap works, keep a RAM copy of the screen for fast scrolling.
pub fn enable_shadow_buffer() {
    let mut console = CONSOLE.lock();
    if let Some(s) = console.screen.as_mut() {
        let size = s.pitch * s.height;
        let buf = vec![0u8; size].leak();
        unsafe { core::ptr::copy_nonoverlapping(s.fb, buf.as_mut_ptr(), size) };
        s.shadow = buf.as_mut_ptr();
    }
}

impl Console {
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        if let Some(s) = self.screen.as_mut() {
            s.draw_cursor(false);
        }
        for &b in bytes {
            if b == b'\n' {
                serial::write_byte(b'\r');
            }
            serial::write_byte(b);
            if let Some(s) = self.screen.as_mut() {
                s.put_byte(b);
            }
        }
        if let Some(s) = self.screen.as_mut() {
            s.draw_cursor(true);
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

pub fn write_bytes(bytes: &[u8]) {
    CONSOLE.lock().write_bytes(bytes);
}

pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    let _ = CONSOLE.lock().write_fmt(args);
}

/// Panic path: take the console even if someone was holding the lock.
pub fn _panic_print(args: fmt::Arguments) {
    use fmt::Write;
    unsafe { CONSOLE.force_unlock() };
    let _ = CONSOLE.lock().write_fmt(args);
}

pub fn size() -> Option<(usize, usize, usize)> {
    CONSOLE.lock().screen.as_ref().map(|s| (s.width, s.height, s.scale))
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => ($crate::console::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! kprintln {
    () => ($crate::kprint!("\n"));
    ($($arg:tt)*) => ($crate::kprint!("{}\n", format_args!($($arg)*)));
}
