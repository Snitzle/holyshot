//! Annotation shapes, in image-pixel coordinates, rendered as GSK nodes.

use gtk::{gdk, graphene, gsk, pango, prelude::*};
use std::cell::RefCell;

use crate::img::Img;
use crate::util::{rect, rgba};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct P {
    pub x: f32,
    pub y: f32,
}

impl P {
    pub const fn new(x: f32, y: f32) -> P {
        P { x, y }
    }
    pub fn len(self) -> f32 {
        self.x.hypot(self.y)
    }
}
impl std::ops::Add for P {
    type Output = P;
    fn add(self, o: P) -> P {
        P::new(self.x + o.x, self.y + o.y)
    }
}
impl std::ops::Sub for P {
    type Output = P;
    fn sub(self, o: P) -> P {
        P::new(self.x - o.x, self.y - o.y)
    }
}
impl std::ops::Mul<f32> for P {
    type Output = P;
    fn mul(self, k: f32) -> P {
        P::new(self.x * k, self.y * k)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Select,
    Arrow,
    Rect,
    Ellipse,
    Pen,
    Highlight,
    Text,
    Number,
    Pixelate,
}

// Size presets [_, S, M, L], multiplied by the capture scale.
const STROKE: [f32; 4] = [0.0, 3.0, 5.0, 9.0];
const HIGHLIGHT: [f32; 4] = [0.0, 14.0, 22.0, 34.0];
pub const TEXT: [f32; 4] = [0.0, 18.0, 28.0, 44.0];
const NUMBER: [f32; 4] = [0.0, 26.0, 36.0, 52.0];
const PIXEL: [f32; 4] = [0.0, 8.0, 14.0, 24.0];

#[derive(Clone)]
pub enum Kind {
    Arrow(P, P),
    Rect(P, P),
    Ellipse(P, P),
    Pixelate(P, P),
    Pen { pts: Vec<P>, straight: bool },
    Highlight { pts: Vec<P>, straight: bool },
    Text { pos: P, text: String },
    Number { c: P, n: u32 },
}

#[derive(Clone, Default)]
struct Cache {
    pixelate: Option<([i32; 5], gdk::Texture)>,
    light: Option<([i32; 4], bool)>,
    text: Option<(String, usize, P)>,
}

#[derive(Clone)]
pub struct Shape {
    pub kind: Kind,
    pub color: gdk::RGBA,
    pub size: usize,
    pub unit: f32,
    pub hidden: bool,
    cache: RefCell<Cache>,
}

/// What shapes need from the document to draw / measure themselves.
pub struct Ctx<'a> {
    pub base: &'a Img,
    pub base_tex: &'a gdk::Texture,
    pub pango: &'a pango::Context,
}

pub fn is_light(c: &gdk::RGBA) -> bool {
    0.299 * c.red() + 0.587 * c.green() + 0.114 * c.blue() > 0.67
}

fn norm(a: P, b: P) -> graphene::Rect {
    rect(a.x.min(b.x), a.y.min(b.y), (a.x - b.x).abs(), (a.y - b.y).abs())
}

fn dist_seg(p: P, a: P, b: P) -> f32 {
    let ab = b - a;
    let l2 = ab.x * ab.x + ab.y * ab.y;
    if l2 < 1e-6 {
        return (p - a).len();
    }
    let t = (((p.x - a.x) * ab.x + (p.y - a.y) * ab.y) / l2).clamp(0.0, 1.0);
    (p - (a + ab * t)).len()
}

pub fn snap45(o: P, p: P) -> P {
    let d = p - o;
    let step = std::f32::consts::FRAC_PI_4;
    let ang = (d.y.atan2(d.x) / step).round() * step;
    o + P::new(ang.cos(), ang.sin()) * d.len()
}

pub fn snap_square(o: P, p: P) -> P {
    let d = p - o;
    let s = d.x.abs().max(d.y.abs());
    o + P::new(s.copysign(if d.x == 0.0 { 1.0 } else { d.x }), s.copysign(if d.y == 0.0 { 1.0 } else { d.y }))
}

fn font(px: f32, bold: bool) -> pango::FontDescription {
    let mut fd = pango::FontDescription::from_string("Sans");
    fd.set_weight(if bold { pango::Weight::Bold } else { pango::Weight::Semibold });
    fd.set_absolute_size((px * pango::SCALE as f32) as f64);
    fd
}

pub fn text_font_px(size: usize, unit: f32) -> f32 {
    TEXT[size] * unit
}

impl Shape {
    pub fn new(kind: Kind, color: gdk::RGBA, size: usize, unit: f32) -> Shape {
        Shape { kind, color, size, unit, hidden: false, cache: RefCell::default() }
    }

    pub fn is_pixelate(&self) -> bool {
        matches!(self.kind, Kind::Pixelate(..))
    }
    pub fn is_highlight(&self) -> bool {
        matches!(self.kind, Kind::Highlight { .. })
    }
    pub fn is_text(&self) -> bool {
        matches!(self.kind, Kind::Text { .. })
    }

    pub fn width(&self) -> f32 {
        match self.kind {
            Kind::Highlight { .. } => HIGHLIGHT[self.size] * self.unit,
            Kind::Arrow(..) | Kind::Rect(..) | Kind::Ellipse(..) | Kind::Pen { .. } => STROKE[self.size] * self.unit,
            _ => 0.0,
        }
    }

    // ---- editing --------------------------------------------------------
    pub fn translate(&mut self, d: P) {
        match &mut self.kind {
            Kind::Arrow(a, b) | Kind::Rect(a, b) | Kind::Ellipse(a, b) | Kind::Pixelate(a, b) => {
                *a = *a + d;
                *b = *b + d;
            }
            Kind::Pen { pts, .. } | Kind::Highlight { pts, .. } => pts.iter_mut().for_each(|p| *p = *p + d),
            Kind::Text { pos, .. } => *pos = *pos + d,
            Kind::Number { c, .. } => *c = *c + d,
        }
    }

    /// Extend the shape while it's being drawn.
    pub fn drag_to(&mut self, p: P, constrain: bool) {
        match &mut self.kind {
            Kind::Arrow(a, b) => *b = if constrain { snap45(*a, p) } else { p },
            Kind::Rect(a, b) | Kind::Ellipse(a, b) | Kind::Pixelate(a, b) => {
                *b = if constrain { snap_square(*a, p) } else { p }
            }
            Kind::Pen { pts, straight } | Kind::Highlight { pts, straight } => {
                if constrain {
                    *straight = true;
                    let first = pts[0];
                    pts.clear();
                    pts.push(first);
                    pts.push(p);
                } else if *straight {
                    *pts.last_mut().unwrap() = p;
                } else if pts.last().is_none_or(|l| (*l - p).len() >= 0.75) {
                    pts.push(p);
                }
            }
            _ => {}
        }
    }

    pub fn is_degenerate(&self) -> bool {
        match &self.kind {
            Kind::Arrow(a, b) => (*b - *a).len() < 4.0,
            Kind::Rect(a, b) | Kind::Ellipse(a, b) | Kind::Pixelate(a, b) => {
                (a.x - b.x).abs() < 3.0 && (a.y - b.y).abs() < 3.0
            }
            Kind::Text { text, .. } => text.trim().is_empty(),
            _ => false,
        }
    }

    pub fn handles(&self) -> Vec<P> {
        match self.kind {
            Kind::Arrow(a, b) => vec![a, b],
            Kind::Rect(a, b) | Kind::Ellipse(a, b) | Kind::Pixelate(a, b) => {
                vec![a, b, P::new(a.x, b.y), P::new(b.x, a.y)]
            }
            _ => vec![],
        }
    }

    pub fn set_handle(&mut self, i: usize, p: P, constrain: bool) {
        match &mut self.kind {
            Kind::Arrow(a, b) => {
                if i == 0 {
                    *a = if constrain { snap45(*b, p) } else { p }
                } else {
                    *b = if constrain { snap45(*a, p) } else { p }
                }
            }
            Kind::Rect(a, b) | Kind::Ellipse(a, b) | Kind::Pixelate(a, b) => match i {
                0 => *a = p,
                1 => *b = p,
                2 => {
                    a.x = p.x;
                    b.y = p.y
                }
                _ => {
                    b.x = p.x;
                    a.y = p.y
                }
            },
            _ => {}
        }
    }

    // ---- geometry -------------------------------------------------------
    fn text_layout(&self, ctx: &Ctx) -> Option<pango::Layout> {
        let Kind::Text { text, .. } = &self.kind else { return None };
        let l = pango::Layout::new(ctx.pango);
        l.set_font_description(Some(&font(text_font_px(self.size, self.unit), false)));
        l.set_text(text);
        Some(l)
    }

    fn text_size(&self, ctx: &Ctx) -> P {
        let Kind::Text { text, .. } = &self.kind else { return P::default() };
        if let Some((t, s, sz)) = &self.cache.borrow().text {
            if t == text && *s == self.size {
                return *sz;
            }
        }
        let (w, h) = self.text_layout(ctx).unwrap().pixel_size();
        let sz = P::new(w as f32, h as f32);
        self.cache.borrow_mut().text = Some((text.clone(), self.size, sz));
        sz
    }

    /// Box used for selection outline and click targets of solid shapes.
    pub fn bounds(&self, ctx: &Ctx) -> graphene::Rect {
        let w = self.width();
        match &self.kind {
            Kind::Arrow(a, b) | Kind::Rect(a, b) | Kind::Ellipse(a, b) | Kind::Pixelate(a, b) => {
                let r = norm(*a, *b);
                rect(r.x() - w, r.y() - w, r.width() + 2.0 * w, r.height() + 2.0 * w)
            }
            Kind::Pen { pts, .. } | Kind::Highlight { pts, .. } => {
                let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                for p in pts {
                    x0 = x0.min(p.x);
                    y0 = y0.min(p.y);
                    x1 = x1.max(p.x);
                    y1 = y1.max(p.y);
                }
                rect(x0 - w, y0 - w, x1 - x0 + 2.0 * w, y1 - y0 + 2.0 * w)
            }
            Kind::Text { pos, .. } => {
                let sz = self.text_size(ctx);
                let pad = text_font_px(self.size, self.unit) * 0.15;
                rect(pos.x - pad, pos.y - pad, sz.x + 2.0 * pad, sz.y + 2.0 * pad)
            }
            Kind::Number { c, .. } => {
                let r = NUMBER[self.size] * self.unit / 2.0;
                rect(c.x - r, c.y - r, 2.0 * r, 2.0 * r)
            }
        }
    }

    pub fn hit(&self, p: P, tol: f32, ctx: &Ctx) -> bool {
        let reach = self.width() / 2.0 + tol;
        match &self.kind {
            Kind::Arrow(a, b) => dist_seg(p, *a, *b) <= reach.max(tol * 1.5),
            Kind::Rect(a, b) => {
                let (tl, br) = (P::new(a.x.min(b.x), a.y.min(b.y)), P::new(a.x.max(b.x), a.y.max(b.y)));
                let (tr, bl) = (P::new(br.x, tl.y), P::new(tl.x, br.y));
                [dist_seg(p, tl, tr), dist_seg(p, tr, br), dist_seg(p, br, bl), dist_seg(p, bl, tl)]
                    .into_iter()
                    .fold(f32::MAX, f32::min)
                    <= reach
            }
            Kind::Ellipse(a, b) => {
                let c = (*a + *b) * 0.5;
                let (rx, ry) = (((a.x - b.x) / 2.0).abs().max(1.0), ((a.y - b.y) / 2.0).abs().max(1.0));
                let d = (((p.x - c.x) / rx).powi(2) + ((p.y - c.y) / ry).powi(2)).sqrt();
                (d - 1.0).abs() * rx.min(ry) <= reach
            }
            Kind::Pen { pts, .. } | Kind::Highlight { pts, .. } => {
                if pts.len() == 1 {
                    return (p - pts[0]).len() <= reach;
                }
                pts.windows(2).any(|s| dist_seg(p, s[0], s[1]) <= reach)
            }
            Kind::Pixelate(..) | Kind::Text { .. } | Kind::Number { .. } => {
                let r = self.bounds(ctx);
                p.x >= r.x() - tol && p.x <= r.x() + r.width() + tol && p.y >= r.y() - tol && p.y <= r.y() + r.height() + tol
            }
        }
    }

    // ---- rendering ------------------------------------------------------
    fn shadow(&self) -> [gsk::Shadow; 1] {
        [gsk::Shadow::new(rgba(0.0, 0.0, 0.0, 0.38), 0.8 * self.unit, 1.4 * self.unit, 2.6 * self.unit)]
    }

    fn stroke(&self, w: f32) -> gsk::Stroke {
        let s = gsk::Stroke::new(w);
        s.set_line_cap(gsk::LineCap::Round);
        s.set_line_join(gsk::LineJoin::Round);
        s
    }

    fn polyline(pts: &[P]) -> gsk::Path {
        let b = gsk::PathBuilder::new();
        b.move_to(pts[0].x, pts[0].y);
        match pts.len() {
            1 => b.line_to(pts[0].x + 0.01, pts[0].y),
            2 => b.line_to(pts[1].x, pts[1].y),
            _ => {
                // smooth: quadratic curves through the midpoints
                for i in 1..pts.len() - 1 {
                    let m = (pts[i] + pts[i + 1]) * 0.5;
                    b.quad_to(pts[i].x, pts[i].y, m.x, m.y);
                }
                let l = pts[pts.len() - 1];
                b.line_to(l.x, l.y);
            }
        }
        b.to_path()
    }

    fn arrow_path(&self, a: P, b: P) -> Option<gsk::Path> {
        let w = self.width();
        let d = b - a;
        let len = d.len();
        if len < 0.5 {
            return None;
        }
        let u = d * (1.0 / len);
        let n = P::new(-u.y, u.x);
        let head_len = (len * 0.65).min(w * 4.2 + 6.0);
        let head_half = head_len * 0.62;
        let neck = b - u * head_len;
        let (tail_half, neck_half) = (w * 0.22, w * 0.62);
        let pts = [
            a + n * tail_half,
            neck + n * neck_half,
            neck + n * head_half,
            b,
            neck - n * head_half,
            neck - n * neck_half,
            a - n * tail_half,
        ];
        let pb = gsk::PathBuilder::new();
        pb.move_to(pts[0].x, pts[0].y);
        for p in &pts[1..] {
            pb.line_to(p.x, p.y);
        }
        pb.close();
        Some(pb.to_path())
    }

    fn backdrop_light(&self, ctx: &Ctx) -> bool {
        let r = self.bounds(ctx);
        let key = [r.x() as i32, r.y() as i32, r.width() as i32, r.height() as i32];
        if let Some((k, v)) = self.cache.borrow().light {
            if k == key {
                return v;
            }
        }
        let v = ctx.base.luminance(r.x().max(0.0) as usize, r.y().max(0.0) as usize, r.width() as usize, r.height() as usize) > 150.0;
        self.cache.borrow_mut().light = Some((key, v));
        v
    }

    pub fn draw(&self, sn: &gtk::Snapshot, ctx: &Ctx) {
        if self.hidden {
            return;
        }
        let c = &self.color;
        match &self.kind {
            Kind::Arrow(a, b) => {
                if let Some(path) = self.arrow_path(*a, *b) {
                    sn.push_shadow(&self.shadow());
                    sn.append_fill(&path, gsk::FillRule::Winding, c);
                    // a hairline stroke rounds off the sharp corners
                    sn.append_stroke(&path, &self.stroke(self.unit.max(1.0)), c);
                    sn.pop();
                }
            }
            Kind::Rect(a, b) => {
                let w = self.width();
                let pb = gsk::PathBuilder::new();
                pb.add_rounded_rect(&gsk::RoundedRect::from_rect(norm(*a, *b), w));
                sn.push_shadow(&self.shadow());
                sn.append_stroke(&pb.to_path(), &self.stroke(w), c);
                sn.pop();
            }
            Kind::Ellipse(a, b) => {
                let r = norm(*a, *b);
                let s = graphene::Size::new(r.width() / 2.0, r.height() / 2.0);
                let pb = gsk::PathBuilder::new();
                pb.add_rounded_rect(&gsk::RoundedRect::new(r, s, s, s, s));
                sn.push_shadow(&self.shadow());
                sn.append_stroke(&pb.to_path(), &self.stroke(self.width()), c);
                sn.pop();
            }
            Kind::Pen { pts, .. } => {
                sn.push_shadow(&self.shadow());
                sn.append_stroke(&Self::polyline(pts), &self.stroke(self.width()), c);
                sn.pop();
            }
            Kind::Highlight { pts, straight } => {
                let path = Self::polyline(pts);
                let stroke = self.stroke(self.width());
                if *straight {
                    stroke.set_line_cap(gsk::LineCap::Square);
                }
                if self.backdrop_light(ctx) {
                    // Real highlighter look: multiply with what's underneath.
                    // (Highlights are drawn straight after the base image, so
                    // the base *is* the backdrop.)
                    let bb = self.bounds(ctx);
                    sn.push_blend(gsk::BlendMode::Multiply);
                    sn.push_clip(&bb);
                    sn.append_texture(ctx.base_tex, &rect(0.0, 0.0, ctx.base.w as f32, ctx.base.h as f32));
                    sn.pop();
                    sn.pop();
                    sn.append_stroke(&path, &stroke, c);
                    sn.pop();
                } else {
                    // Multiply would vanish on dark backgrounds.
                    let mut t = *c;
                    t.set_alpha(0.42);
                    sn.append_stroke(&path, &stroke, &t);
                }
            }
            Kind::Pixelate(a, b) => self.draw_pixelate(sn, ctx, *a, *b),
            Kind::Text { pos, .. } => {
                let layout = self.text_layout(ctx).unwrap();
                let pb = gsk::PathBuilder::new();
                pb.add_layout(&layout);
                let path = pb.to_path();
                let halo = if is_light(c) { rgba(0.08, 0.08, 0.08, 0.86) } else { rgba(1.0, 1.0, 1.0, 0.92) };
                sn.save();
                sn.translate(&graphene::Point::new(pos.x, pos.y));
                sn.append_stroke(&path, &self.stroke(text_font_px(self.size, self.unit) * 0.16), &halo);
                sn.append_fill(&path, gsk::FillRule::Winding, c);
                sn.restore();
            }
            Kind::Number { c: center, n } => {
                let d = NUMBER[self.size] * self.unit;
                let light = is_light(c);
                let pb = gsk::PathBuilder::new();
                pb.add_circle(&graphene::Point::new(center.x, center.y), d / 2.0 - self.unit);
                let circle = pb.to_path();
                let edge = if light { rgba(0.12, 0.12, 0.12, 1.0) } else { gdk::RGBA::WHITE };
                sn.push_shadow(&self.shadow());
                sn.append_fill(&circle, gsk::FillRule::Winding, c);
                sn.append_stroke(&circle, &self.stroke((self.unit * 2.0).max(1.5)), &edge);
                sn.pop();
                let l = pango::Layout::new(ctx.pango);
                l.set_font_description(Some(&font(d * if *n < 10 { 0.55 } else { 0.45 }, true)));
                l.set_text(&n.to_string());
                let (lw, lh) = l.pixel_size();
                sn.save();
                sn.translate(&graphene::Point::new(center.x - lw as f32 / 2.0, center.y - lh as f32 / 2.0));
                sn.append_layout(&l, &if light { rgba(0.08, 0.08, 0.08, 1.0) } else { gdk::RGBA::WHITE });
                sn.restore();
            }
        }
    }

    fn draw_pixelate(&self, sn: &gtk::Snapshot, ctx: &Ctx, a: P, b: P) {
        let r = norm(a, b);
        let (bw, bh) = (ctx.base.w as f32, ctx.base.h as f32);
        let x0 = r.x().clamp(0.0, bw) as usize;
        let y0 = r.y().clamp(0.0, bh) as usize;
        let x1 = (r.x() + r.width()).clamp(0.0, bw) as usize;
        let y1 = (r.y() + r.height()).clamp(0.0, bh) as usize;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let block = (PIXEL[self.size] * self.unit).round().max(2.0) as usize;
        let key = [x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32, block as i32];
        let tex = {
            let cached = self.cache.borrow().pixelate.clone();
            match cached {
                Some((k, t)) if k == key => t,
                _ => {
                    let (px, cols, rows) = ctx.base.pixelate(x0, y0, x1 - x0, y1 - y0, block);
                    let t: gdk::Texture = gdk::MemoryTexture::new(
                        cols as i32,
                        rows as i32,
                        crate::img::FORMAT,
                        &gtk::glib::Bytes::from_owned(px),
                        cols * 4,
                    )
                    .upcast();
                    self.cache.borrow_mut().pixelate = Some((key, t.clone()));
                    t
                }
            }
        };
        let area = rect(x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32);
        sn.push_clip(&area);
        sn.append_scaled_texture(
            &tex,
            gsk::ScalingFilter::Nearest,
            &rect(x0 as f32, y0 as f32, (tex.width() as usize * block) as f32, (tex.height() as usize * block) as f32),
        );
        sn.pop();
    }
}

/// Draw a document: base image, then highlights (which blend with it),
/// pixelation, then everything else in creation order.
pub fn render(sn: &gtk::Snapshot, ctx: &Ctx, shapes: &[Shape], active: Option<&Shape>, filter: gsk::ScalingFilter) {
    let full = rect(0.0, 0.0, ctx.base.w as f32, ctx.base.h as f32);
    sn.append_scaled_texture(ctx.base_tex, filter, &full);
    let all = || shapes.iter().chain(active);
    all().filter(|s| s.is_highlight()).for_each(|s| s.draw(sn, ctx));
    all().filter(|s| s.is_pixelate()).for_each(|s| s.draw(sn, ctx));
    all().filter(|s| !s.is_highlight() && !s.is_pixelate()).for_each(|s| s.draw(sn, ctx));
}
