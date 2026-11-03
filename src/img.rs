//! A BGRA (premultiplied) pixel buffer shared between threads and GTK.

use gtk::{gdk, glib, prelude::*};
use std::sync::Arc;

/// Pixel storage. Captures and crops are `Owned` (reference-counted) so the
/// save thread can take the buffer back and encode it in place once it's the
/// last user; images decoded by GTK stay as GTK's `Bytes`.
#[derive(Clone)]
enum Buf {
    Owned(Arc<Vec<u8>>, usize), // data, byte offset (for zero-copy views)
    Gtk(glib::Bytes),
}

/// Lets GTK textures borrow an `Owned` buffer without copying.
struct Share(Arc<Vec<u8>>, usize, usize);
impl AsRef<[u8]> for Share {
    fn as_ref(&self) -> &[u8] {
        &self.0[self.1..self.2]
    }
}

#[derive(Clone)]
pub struct Img {
    buf: Buf,
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
    /// An opaque image owning `data` (tightly packed or with `stride`).
    pub fn from_vec(data: Vec<u8>, w: usize, h: usize, stride: usize, scale: f64) -> Self {
        Img { buf: Buf::Owned(Arc::new(data), 0), w, h, stride, scale, opaque: true }
    }

    fn owned(data: Vec<u8>, w: usize, h: usize, like: &Img) -> Img {
        Img { buf: Buf::Owned(Arc::new(data), 0), w, h, stride: w * 4, ..like.clone() }
    }

    /// The pixel bytes (first row first; rows are `stride` apart).
    pub fn data(&self) -> &[u8] {
        let len = (self.h - 1) * self.stride + self.w * 4;
        match &self.buf {
            Buf::Owned(v, off) => &v[*off..*off + len],
            Buf::Gtk(b) => &b[..len],
        }
    }

    fn gbytes(&self) -> glib::Bytes {
        match &self.buf {
            Buf::Owned(v, off) => {
                let len = (self.h - 1) * self.stride + self.w * 4;
                glib::Bytes::from_owned(Share(v.clone(), *off, *off + len))
            }
            Buf::Gtk(b) => b.clone(),
        }
    }

    /// Wraps the pixels as a texture without copying them.
    pub fn texture(&self) -> gdk::Texture {
        // (Premultiplied BGRA is Cairo's native layout: measured cheaper than
        // tagging opaque images BGRx, which forces a per-pixel conversion.)
        gdk::MemoryTexture::new(self.w as i32, self.h as i32, FORMAT, &self.gbytes(), self.stride).upcast()
    }

    /// A view of a sub-rectangle sharing the same memory (no copy).
    pub fn view(&self, x: usize, y: usize, w: usize, h: usize) -> Img {
        let start = y * self.stride + x * 4;
        let buf = match &self.buf {
            Buf::Owned(v, off) => Buf::Owned(v.clone(), off + start),
            Buf::Gtk(b) => Buf::Gtk(glib::Bytes::from_bytes(b, start..start + (h - 1) * self.stride + w * 4)),
        };
        Img { buf, w, h, ..self.clone() }
    }

    /// A compact copy of a sub-rectangle.
    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Img {
        let mut out = Vec::with_capacity(w * h * 4);
        for row in y..y + h {
            out.extend_from_slice(&self.row(row)[x * 4..(x + w) * 4]);
        }
        Img::owned(out, w, h, self)
    }

    pub fn from_texture(tex: &gdk::Texture, scale: f64) -> Img {
        let mut dl = gdk::TextureDownloader::new(tex);
        dl.set_format(FORMAT);
        let (bytes, stride) = dl.download_bytes();
        let mut img =
            Img { buf: Buf::Gtk(bytes), w: tex.width() as usize, h: tex.height() as usize, stride, scale, opaque: false };
        img.opaque = img.check_opaque();
        img
    }

    /// High-quality downscale: repeated 2x2 box halving (mipmapping) while
    /// the image is more than twice the target, then one area-averaging
    /// pass. Written so LLVM vectorises the inner loops; ~10x cheaper than a
    /// direct area filter on a 5 MP screenshot. The CPU renderer is never
    /// asked to resample big images per frame (60 ms of CPU each).
    pub fn downscale(&self, tw: usize, th: usize) -> Img {
        let (tw, th) = (tw.clamp(1, self.w), th.clamp(1, self.h));
        if tw == self.w && th == self.h {
            return self.clone();
        }
        let mut cur = std::borrow::Cow::Borrowed(self);
        while cur.w >= 2 * tw && cur.h >= 2 * th {
            cur = std::borrow::Cow::Owned(cur.halve());
        }
        if cur.w == tw && cur.h == th {
            return cur.into_owned();
        }
        cur.area_resize(tw, th)
    }

    fn halve(&self) -> Img {
        // Packed-pixel ("SWAR") averaging: two BGRA pixels per u64, all
        // channels at once. ~12 instructions per output pixel.
        #[inline(always)]
        fn avg64(p: u64, q: u64) -> u64 {
            (p & q) + (((p ^ q) & 0xfefe_fefe_fefe_fefe) >> 1)
        }
        #[inline(always)]
        fn avg32(p: u32, q: u32) -> u32 {
            (p & q) + (((p ^ q) & 0xfefe_fefe) >> 1)
        }
        let (w, h) = (self.w / 2, self.h / 2);
        let mut out = vec![0u8; w * h * 4];
        for (y, dst) in out.chunks_exact_mut(w * 4).enumerate() {
            let a = &self.row(2 * y)[..w * 8];
            let b = &self.row(2 * y + 1)[..w * 8];
            for ((d, a), b) in dst.chunks_exact_mut(4).zip(a.chunks_exact(8)).zip(b.chunks_exact(8)) {
                let v = avg64(u64::from_ne_bytes(a.try_into().unwrap()), u64::from_ne_bytes(b.try_into().unwrap()));
                d.copy_from_slice(&avg32(v as u32, (v >> 32) as u32).to_ne_bytes());
            }
        }
        Img::owned(out, w, h, self)
    }

    /// Area averaging with fractional edge weights (box filter), separable.
    /// Only called with ratios below 2 (after halving), so every output pixel
    /// has at most 3 source taps: fixed-size loops with no slicing, and 16-bit
    /// arithmetic (weights sum to 256) that vectorises even with plain SSE2.
    fn area_resize(&self, tw: usize, th: usize) -> Img {
        let hx = Taps3::new(self.w, tw);
        let hy = Taps3::new(self.h, th);
        // horizontal: widen each pixel to 4 x 16-bit lanes in a u64, so a tap
        // is one multiply-add for all channels.
        #[inline(always)]
        fn widen(p: &[u8]) -> u64 {
            p[0] as u64 | (p[1] as u64) << 16 | (p[2] as u64) << 32 | (p[3] as u64) << 48
        }
        let row_out = tw * 4;
        let mut mid = vec![0u8; row_out * self.h];
        let mut wide = vec![0u64; self.w + 2];
        for (y, dst) in mid.chunks_exact_mut(row_out).enumerate() {
            for (w, p) in wide.iter_mut().zip(self.row(y).chunks_exact(4)) {
                *w = widen(p);
            }
            for (d, t) in dst.chunks_exact_mut(4).zip(&hx.taps) {
                let s = t.0 as usize;
                let v = (wide[s] * t.1[0] as u64 + wide[s + 1] * t.1[1] as u64 + wide[s + 2] * t.1[2] as u64
                    + 0x0080_0080_0080_0080)
                    >> 8;
                d.copy_from_slice(&[v as u8, (v >> 16) as u8, (v >> 32) as u8, (v >> 48) as u8]);
            }
        }
        // vertical: three source rows per output row, 16-bit lanes
        let mut out = vec![0u8; row_out * th];
        let last = self.h - 1;
        for (dst, t) in out.chunks_exact_mut(row_out).zip(&hy.taps) {
            let s = t.0 as usize;
            let r = |k: usize| &mid[(s + k).min(last) * row_out..][..row_out];
            let (a, b, c) = (r(0), r(1), r(2));
            let [w0, w1, w2] = t.1;
            for (((d, &a), &b), &c) in dst.iter_mut().zip(a).zip(b).zip(c) {
                *d = ((a as u16 * w0 + b as u16 * w1 + c as u16 * w2 + 128) >> 8) as u8;
            }
        }
        Img::owned(out, tw, th, self)
    }

    pub fn row(&self, y: usize) -> &[u8] {
        &self.data()[y * self.stride..y * self.stride + self.w * 4]
    }

    fn check_opaque(&self) -> bool {
        (0..self.h).all(|y| self.row(y).chunks_exact(4).all(|p| p[3] == 255))
    }

    /// The PNG scanlines (filter byte 0 = "none", then RGB(A)) built in place
    /// over our own buffer when we're its only user, so encoding needs no
    /// second full-size buffer; otherwise a copy.
    fn into_scanlines(self) -> (Vec<u8>, bool) {
        let opaque = self.opaque || self.check_opaque();
        if let (true, Buf::Owned(arc, 0)) = (opaque, &self.buf) {
            if self.stride >= self.w * 4 && Arc::strong_count(arc) == 1 {
                let (w, h, stride) = (self.w, self.h, self.stride);
                let Buf::Owned(arc, _) = self.buf else { unreachable!() };
                if let Ok(mut v) = Arc::try_unwrap(arc) {
                    // Row y moves from y*stride to y*(1+3w) <= y*stride; within a
                    // row each pixel is read before anything overwrites it.
                    let row_len = 1 + 3 * w;
                    for y in 0..h {
                        let (src, dst) = (y * stride, y * row_len);
                        if dst + row_len <= src {
                            // from row ~4 on the rows no longer overlap: split
                            // the buffer and convert with plain (vectorised) slices
                            let (lo, hi) = v.split_at_mut(src);
                            let (d, sp) = (&mut lo[dst..dst + row_len], &hi[..w * 4]);
                            d[0] = 0;
                            for (d, s) in d[1..].chunks_exact_mut(3).zip(sp.chunks_exact(4)) {
                                d[0] = s[2];
                                d[1] = s[1];
                                d[2] = s[0];
                            }
                        } else {
                            let p0 = [v[src + 2], v[src + 1], v[src]];
                            v[dst] = 0;
                            v[dst + 1..dst + 4].copy_from_slice(&p0);
                            for x in 1..w {
                                let s = src + 4 * x;
                                let (r, g, b) = (v[s + 2], v[s + 1], v[s]);
                                let d = dst + 1 + 3 * x;
                                v[d] = r;
                                v[d + 1] = g;
                                v[d + 2] = b;
                            }
                        }
                    }
                    v.truncate(row_len * h);
                    crate::util::trace("png: scanlines built in place");
                    return (v, true);
                }
                unreachable!("strong_count was 1");
            }
        }
        crate::util::trace("png: scanlines copied");
        (self.scanlines(opaque), opaque)
    }

    fn scanlines(&self, opaque: bool) -> Vec<u8> {
        let bpp = if opaque { 3 } else { 4 };
        // filter byte 0 ("none") + RGB(A) per row: screenshots are mostly flat
        // colour, where unfiltered rows beat every PNG filter (benchmarked).
        let row_len = 1 + self.w * bpp;
        let mut raw = vec![0u8; row_len * self.h];
        for (y, line) in raw.chunks_exact_mut(row_len).enumerate() {
            let src = self.row(y);
            let dst = &mut line[1..];
            if opaque {
                for (d, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                    d.copy_from_slice(&[s[2], s[1], s[0]]);
                }
            } else {
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    let a = s[3] as u32;
                    let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a / 2) / a).min(255) as u8 };
                    d.copy_from_slice(&[un(s[2]), un(s[1]), un(s[0]), s[3]]);
                }
            }
        }
        raw
    }

    /// Encodes as PNG with libdeflate (see `deflate` below). Consumes the
    /// image so the buffer can be reused in place; `level`: libdeflate 1-12.
    pub fn into_png(self, level: i32) -> Result<Vec<u8>, String> {
        let (w, h) = (self.w, self.h);
        let (raw, opaque) = self.into_scanlines();
        let z = deflate::zlib(&raw, level).ok_or("compression failed")?;
        drop(raw);
        png_file(w, h, opaque, &z)
    }

    pub fn encode_png(&self, level: i32) -> Result<Vec<u8>, String> {
        self.clone_detached().into_png(level)
    }

    /// A clone that doesn't count as another user of an owned buffer (so
    /// `into_png` on it copies rather than steals).
    fn clone_detached(&self) -> Img {
        match &self.buf {
            Buf::Owned(..) => Img { buf: Buf::Gtk(self.gbytes()), ..self.clone() },
            Buf::Gtk(_) => self.clone(),
        }
    }

    /// Writes atomically (temp file + rename) so a half-written file is never seen.
    pub fn save_png(self, path: &std::path::Path) -> Result<(), String> {
        let png = self.into_png(deflate::LEVEL)?;
        let tmp = path.with_extension("png.part");
        let res = std::fs::write(&tmp, &png).and_then(|_| std::fs::rename(&tmp, path));
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res.map_err(|e| e.to_string())
    }
}

fn png_file(w: usize, h: usize, opaque: bool, z: &[u8]) -> Result<Vec<u8>, String> {
    {
        let mut out = Vec::with_capacity(z.len() + 64);
        out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&(w as u32).to_be_bytes());
        ihdr.extend_from_slice(&(h as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, if opaque { 2 } else { 6 }, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", z);
        chunk(&mut out, b"IEND", &[]);
        Ok(out)
    }
}

impl Img {
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

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = deflate::crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// libdeflate, via the system library (no -devel package needed).
///
/// Benchmarked on a real 3440x1440 screenshot (`holyshot __bench-png`), in
/// retired instructions (deterministic) / output size:
///   zlib level 6 (png crate default)   1294 M  718 KB
///   zlib level 3                        992 M  727 KB
///   zlib level 1                        379 M  965 KB
///   libdeflate level 1                  231 M  718 KB   <- used
///   libdeflate level 6                  483 M  673 KB
/// i.e. the old default's file size for 18% of the CPU. The cost: it needs the
/// whole image in one buffer (~15 MB, briefly) instead of streaming rows.
pub mod deflate {
    use std::ffi::{c_int, c_void};

    pub const LEVEL: i32 = 1;

    #[link(name = "libdeflate.so.0", kind = "dylib", modifiers = "+verbatim")]
    unsafe extern "C" {
        fn libdeflate_alloc_compressor(level: c_int) -> *mut c_void;
        fn libdeflate_free_compressor(c: *mut c_void);
        fn libdeflate_zlib_compress_bound(c: *mut c_void, n: usize) -> usize;
        fn libdeflate_zlib_compress(c: *mut c_void, input: *const c_void, n: usize, out: *mut c_void, avail: usize) -> usize;
        fn libdeflate_crc32(crc: u32, buf: *const c_void, len: usize) -> u32;
    }

    pub fn zlib(input: &[u8], level: i32) -> Option<Vec<u8>> {
        // SAFETY: plain C API; buffers are valid for the lengths passed, and
        // the compressor is freed on every path.
        unsafe {
            let c = libdeflate_alloc_compressor(level);
            if c.is_null() {
                return None;
            }
            let bound = libdeflate_zlib_compress_bound(c, input.len());
            // Only the pages actually written become resident.
            let mut out = Vec::<u8>::with_capacity(bound);
            let n = libdeflate_zlib_compress(c, input.as_ptr().cast(), input.len(), out.as_mut_ptr().cast(), bound);
            libdeflate_free_compressor(c);
            if n == 0 {
                return None;
            }
            out.set_len(n);
            out.shrink_to_fit();
            Some(out)
        }
    }

    pub fn crc32(data: &[u8]) -> u32 {
        // SAFETY: reads `data.len()` bytes from a valid slice
        unsafe { libdeflate_crc32(0, data.as_ptr().cast(), data.len()) }
    }
}

/// `holyshot __bench-png FILE...`: instructions / time / size per libdeflate level.
pub fn bench_png(img: &Img, name: &str) {
    // KWin captures are opaque; benchmark that path even if the file isn't.
    let img = &Img { opaque: true, ..img.clone() };
    println!("{name} ({}x{}, encoded as RGB)", img.w, img.h);
    for level in [1, 2, 4, 6, 9] {
        let i0 = crate::util::instructions();
        let t = std::time::Instant::now();
        let png = img.encode_png(level).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let mi = (crate::util::instructions() - i0) as f64 / 1e6;
        println!("  libdeflate-{level:<2} {mi:8.1} M instr {ms:7.1} ms {:8.1} KB", png.len() as f64 / 1024.0);
    }
    // lossless round trip through GTK's own PNG decoder, for the copying
    // paths and the in-place path (an owned buffer with a single user)
    let owned = || Img::from_vec(img.data().to_vec(), img.w, img.h, img.stride, img.scale);
    for (label, src, steal) in
        [("RGB", img.clone(), false), ("RGBA", Img { opaque: false, ..img.clone() }, false), ("RGB in-place", owned(), true)]
    {
        let png = if steal { owned().into_png(1).unwrap() } else { src.encode_png(1).unwrap() };
        let back = gdk::Texture::from_bytes(&glib::Bytes::from_owned(png)).map(|t| Img::from_texture(&t, 1.0));
        let ok = back.is_ok_and(|b| {
            (0..src.h).all(|y| {
                let (a, c) = (src.row(y), b.row(y));
                a.chunks_exact(4).zip(c.chunks_exact(4)).all(|(p, q)| {
                    if label != "RGBA" { p[..3] == q[..3] } else { p[3] == q[3] && (p[3] < 250 || p[..3] == q[..3]) }
                })
            })
        });
        println!("  round trip {label}: {}", if ok { "pixel-identical" } else { "MISMATCH" });
    }
}

/// Per output pixel: first source index and three weights summing to 256
/// (box filter with fractional edges; ratio must be below 2).
struct Taps3 {
    taps: Vec<(u32, [u16; 3])>,
}

impl Taps3 {
    fn new(src: usize, dst: usize) -> Taps3 {
        let k = src as f64 / dst as f64;
        debug_assert!(k < 2.0);
        let taps = (0..dst)
            .map(|o| {
                let (a, b) = (o as f64 * k, ((o + 1) as f64 * k).min(src as f64));
                let i0 = a.floor() as usize;
                let mut w = [0u16; 3];
                for (j, wj) in w.iter_mut().enumerate() {
                    let i = (i0 + j) as f64;
                    let cover = (b.min(i + 1.0) - a.max(i)).max(0.0);
                    *wj = (cover / (b - a) * 256.0).round() as u16;
                }
                // rounding error onto the heaviest tap: weights sum to exactly 256
                let sum: i32 = w.iter().map(|&x| x as i32).sum();
                let m = (0..3).max_by_key(|&j| w[j]).unwrap();
                w[m] = (w[m] as i32 + 256 - sum) as u16;
                (i0.min(src - 1) as u32, w)
            })
            .collect();
        Taps3 { taps }
    }
}
