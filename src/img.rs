//! A BGRA (premultiplied) pixel buffer shared between threads and GTK.

use gtk::{gdk, glib, prelude::*};
use std::io::Write;

#[derive(Clone)]
pub struct Img {
    pub bytes: glib::Bytes,
    pub w: usize,
    pub h: usize,
    pub stride: usize,
    /// Screen scale the image was captured at (1.15 => 115%).
    pub scale: f64,
    /// Every alpha byte is 255 (always true for screen captures).
    pub opaque: bool,
}

pub const FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::B8g8r8a8Premultiplied;

impl Img {
    pub fn new(bytes: glib::Bytes, w: usize, h: usize, stride: usize, scale: f64) -> Self {
        Img { bytes, w, h, stride, scale, opaque: true }
    }

    /// Wraps the pixels as a texture without copying them. Opaque images are
    /// tagged as such (BGRx), which lets the Cairo renderer copy them instead
    /// of alpha-blending every pixel (measured: overlay frame 35 -> see notes).
    pub fn texture(&self) -> gdk::Texture {
        let fmt = if self.opaque { gdk::MemoryFormat::B8g8r8x8 } else { FORMAT };
        gdk::MemoryTexture::new(self.w as i32, self.h as i32, fmt, &self.bytes, self.stride).upcast()
    }

    /// A view of a sub-rectangle sharing the same memory (no copy).
    pub fn view(&self, x: usize, y: usize, w: usize, h: usize) -> Img {
        let start = y * self.stride + x * 4;
        let end = start + (h - 1) * self.stride + w * 4;
        Img { bytes: glib::Bytes::from_bytes(&self.bytes, start..end), w, h, stride: self.stride, ..self.clone() }
    }

    /// A compact copy of a sub-rectangle.
    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Img {
        let src: &[u8] = &self.bytes;
        let mut out = Vec::with_capacity(w * h * 4);
        for row in y..y + h {
            let s = row * self.stride + x * 4;
            out.extend_from_slice(&src[s..s + w * 4]);
        }
        Img { bytes: glib::Bytes::from_owned(out), w, h, stride: w * 4, ..self.clone() }
    }

    pub fn from_texture(tex: &gdk::Texture, scale: f64) -> Img {
        let mut dl = gdk::TextureDownloader::new(tex);
        dl.set_format(FORMAT);
        let (bytes, stride) = dl.download_bytes();
        let mut img = Img { bytes, w: tex.width() as usize, h: tex.height() as usize, stride, scale, opaque: false };
        img.opaque = img.check_opaque();
        img
    }

    fn row(&self, y: usize) -> &[u8] {
        &self.bytes[y * self.stride..y * self.stride + self.w * 4]
    }

    fn check_opaque(&self) -> bool {
        (0..self.h).all(|y| self.row(y).chunks_exact(4).all(|p| p[3] == 255))
    }

    /// Streams the image out as PNG one row at a time, so encoding never
    /// needs a second full-size buffer.
    ///
    /// Settings: see `util::png_compression`.
    pub fn write_png(&self, out: impl Write) -> Result<(), String> {
        self.write_png_with(out, crate::util::png_compression(), crate::util::png_filter())
    }

    pub fn write_png_with(&self, out: impl Write, comp: png::Compression, filter: png::Filter) -> Result<(), String> {
        let opaque = self.opaque || self.check_opaque();
        let mut enc = png::Encoder::new(out, self.w as u32, self.h as u32);
        enc.set_color(if opaque { png::ColorType::Rgb } else { png::ColorType::Rgba });
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(comp);
        enc.set_filter(filter);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        let mut stream = writer.stream_writer().map_err(|e| e.to_string())?;
        let mut line = vec![0u8; self.w * if opaque { 3 } else { 4 }];
        for y in 0..self.h {
            let src = self.row(y);
            if opaque {
                for (d, s) in line.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                }
            } else {
                for (d, s) in line.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    let a = s[3] as u32;
                    let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a / 2) / a).min(255) as u8 };
                    d[0] = un(s[2]);
                    d[1] = un(s[1]);
                    d[2] = un(s[0]);
                    d[3] = s[3];
                }
            }
            stream.write_all(&line).map_err(|e| e.to_string())?;
        }
        stream.finish().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Writes atomically (temp file + rename) so a half-written file is never seen.
    pub fn save_png(&self, path: &std::path::Path) -> Result<(), String> {
        let tmp = path.with_extension("png.part");
        let f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let res = self.write_png(std::io::BufWriter::with_capacity(1 << 20, f));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    /// Average luminance (0-255) of a region, sampled sparsely.
    pub fn luminance(&self, x0: usize, y0: usize, w: usize, h: usize) -> f32 {
        let (x1, y1) = ((x0 + w).min(self.w), (y0 + h).min(self.h));
        if x0 >= x1 || y0 >= y1 {
            return 255.0;
        }
        let step = (((x1 - x0) * (y1 - y0)) as f32 / 1500.0).sqrt().max(1.0) as usize;
        let (mut sum, mut n) = (0f32, 0f32);
        for y in (y0..y1).step_by(step) {
            let row = self.row(y);
            for x in (x0..x1).step_by(step) {
                let p = &row[x * 4..x * 4 + 4];
                sum += 0.114 * p[0] as f32 + 0.587 * p[1] as f32 + 0.299 * p[2] as f32;
                n += 1.0;
            }
        }
        sum / n
    }

    /// Block-averaged copy of a region (for the pixelate tool): one output
    /// pixel per `block`-sized cell.
    pub fn pixelate(&self, x0: usize, y0: usize, w: usize, h: usize, block: usize) -> (Vec<u8>, usize, usize) {
        let cols = w.div_ceil(block);
        let rows = h.div_ceil(block);
        let mut out = vec![0u8; cols * rows * 4];
        let sub = (block / 4).max(1); // sample a 4x4-ish grid per block
        for by in 0..rows {
            for bx in 0..cols {
                let mut acc = [0u32; 4];
                let mut n = 0u32;
                let ys = y0 + by * block;
                let xs = x0 + bx * block;
                let mut y = ys;
                while y < (ys + block).min(y0 + h).min(self.h) {
                    let row = self.row(y);
                    let mut x = xs;
                    while x < (xs + block).min(x0 + w).min(self.w) {
                        for c in 0..4 {
                            acc[c] += row[x * 4 + c] as u32;
                        }
                        n += 1;
                        x += sub;
                    }
                    y += sub;
                }
                let o = (by * cols + bx) * 4;
                for c in 0..4 {
                    out[o + c] = if n > 0 { (acc[c] / n) as u8 } else { 0 };
                }
            }
        }
        (out, cols, rows)
    }
}

/// `holyshot __bench-png FILE...`: time/size of each encoder setting.
pub fn bench_png(img: &Img, name: &str) {
    use png::{Compression as C, Filter as F};
    println!("{name} ({}x{})", img.w, img.h);
    for c in [C::Fastest, C::Fast, C::Balanced, C::High] {
        for f in [F::NoFilter, F::Sub, F::Up, F::Paeth, F::Adaptive, F::MinEntropy] {
            let mut best = f64::MAX;
            let mut size = 0;
            for _ in 0..3 {
                let mut buf = Vec::with_capacity(8 << 20);
                let t = std::time::Instant::now();
                img.write_png_with(&mut buf, c, f).unwrap();
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
                size = buf.len();
            }
            println!("  {c:<12?} {f:<11?} {best:7.1} ms  {:8.1} KB", size as f64 / 1024.0);
        }
    }
}
