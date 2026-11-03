//! Full-screen region picker: one layer-shell overlay per monitor showing the
//! frozen, dimmed capture. Drag a box, then move/resize it at leisure;
//! confirm with Enter, double-click or the Capture button.

use gtk::{gdk, glib, graphene, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use crate::draw::DrawArea;
use crate::img::Img;
use crate::util::{self, rect, rgba};
use crate::Hs;

const MIN_SIZE: f32 = 4.0;
const HANDLE: f32 = 5.0;
const HANDLE_HIT: f32 = 10.0;
const BTN_W: f32 = 96.0;
const BTN_H: f32 = 30.0;
const BTN_GAP: f32 = 8.0;

#[derive(Clone, Copy, PartialEq, Debug)]
struct R {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl R {
    fn from_pts(ax: f32, ay: f32, bx: f32, by: f32) -> R {
        R { x0: ax.min(bx), y0: ay.min(by), x1: ax.max(bx), y1: ay.max(by) }
    }
    fn w(&self) -> f32 {
        self.x1 - self.x0
    }
    fn h(&self) -> f32 {
        self.y1 - self.y0
    }
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
    fn moved(&self, dx: f32, dy: f32) -> R {
        R { x0: self.x0 + dx, y0: self.y0 + dy, x1: self.x1 + dx, y1: self.y1 + dy }
    }
    fn g(&self) -> graphene::Rect {
        rect(self.x0, self.y0, self.w(), self.h())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    New,
    Move,
    Resize(u8), // bitmask: 1=left 2=top 4=right 8=bottom
    Button(Btn),
}

#[derive(Clone, Copy, PartialEq)]
enum Btn {
    Capture,
    Cancel,
}

struct State {
    sel: Option<R>,
    mode: Mode,
    press: (f32, f32),
    start: Option<R>,
    mouse: Option<(f32, f32)>,
    last_click: Option<(std::time::Instant, f32, f32)>,
}

struct Ov {
    win: gtk::Window,
    area: DrawArea,
    monitor: gdk::Monitor,
    img: Img,
    tex: gdk::Texture,
    st: RefCell<State>,
    picker: Weak<Picker>,
}

pub struct Picker {
    hs: Weak<Hs>,
    ovs: RefCell<Vec<Rc<Ov>>>,
    finished: Cell<bool>,
}

const HANDLES: [(u8, f32, f32); 8] = [
    (1 | 2, 0.0, 0.0),
    (2, 0.5, 0.0),
    (4 | 2, 1.0, 0.0),
    (4, 1.0, 0.5),
    (4 | 8, 1.0, 1.0),
    (8, 0.5, 1.0),
    (1 | 8, 0.0, 1.0),
    (1, 0.0, 0.5),
];

fn cursor_for(mask: u8) -> &'static str {
    match mask {
        3 | 12 => "nwse-resize",
        6 | 9 => "nesw-resize",
        2 | 8 => "ns-resize",
        _ => "ew-resize",
    }
}

impl Picker {
    pub fn start(hs: &Rc<Hs>, img: Img) -> Rc<Picker> {
        let picker = Rc::new(Picker { hs: Rc::downgrade(hs), ovs: RefCell::default(), finished: Cell::new(false) });
        let mons = util::monitors();
        // The workspace capture spans all monitors; slice it per monitor
        // (zero-copy views into the same buffer).
        let ox = mons.iter().map(|m| m.geometry().x()).min().unwrap_or(0);
        let oy = mons.iter().map(|m| m.geometry().y()).min().unwrap_or(0);
        let s = img.scale;
        for m in &mons {
            let g = m.geometry();
            let px = (((g.x() - ox) as f64 * s).round() as usize).min(img.w.saturating_sub(1));
            let py = (((g.y() - oy) as f64 * s).round() as usize).min(img.h.saturating_sub(1));
            let pw = ((g.width() as f64 * s).round() as usize).clamp(1, img.w - px);
            let ph = ((g.height() as f64 * s).round() as usize).clamp(1, img.h - py);
            let ov = Ov::new(hs, &picker, m, img.view(px, py, pw, ph));
            picker.ovs.borrow_mut().push(ov);
        }
        for ov in picker.ovs.borrow().iter() {
            util::trace_first_paint(&ov.win, "overlay: first paint");
            ov.win.present();
        }
        util::trace("overlay presented");
        if let Some(ms) = util::bench_close_ms() {
            let w = Rc::downgrade(&picker);
            glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
                if let Some(p) = w.upgrade() {
                    p.close();
                }
            });
        }
        picker
    }

    /// Close without capturing (shortcut pressed again).
    pub fn close(&self) {
        self.finish(None);
    }

    fn finish(&self, result: Option<(Img, gdk::Monitor)>) {
        if self.finished.replace(true) {
            return;
        }
        let ovs = std::mem::take(&mut *self.ovs.borrow_mut());
        if let Some(hs) = self.hs.upgrade() {
            // Show the preview first so the app always has a window.
            hs.picked(result);
        }
        for ov in ovs {
            ov.win.destroy();
        }
    }

    fn clear_others(&self, keep: &Ov) {
        for ov in self.ovs.borrow().iter() {
            if !std::ptr::eq(ov.as_ref(), keep) {
                ov.st.borrow_mut().sel = None;
                ov.area.queue_draw();
            }
        }
    }

    fn confirm_any(&self, focused: &Ov) {
        // Enter goes to whichever overlay has keyboard focus, which may not be
        // where the box is: capture the box, else the screen under the mouse.
        let ovs = self.ovs.borrow().clone();
        let target = ovs
            .iter()
            .find(|o| o.st.borrow().sel.is_some())
            .or_else(|| ovs.iter().find(|o| o.st.borrow().mouse.is_some()))
            .cloned();
        match target {
            Some(o) => o.confirm(),
            None => focused.confirm(),
        }
    }
}

impl Ov {
    fn new(hs: &Rc<Hs>, picker: &Rc<Picker>, monitor: &gdk::Monitor, img: Img) -> Rc<Ov> {
        let win = gtk::Window::new();
        win.set_application(Some(&hs.app));
        win.init_layer_shell();
        win.set_layer(Layer::Overlay);
        win.set_namespace(Some("holyshot-picker"));
        win.set_monitor(Some(monitor));
        for e in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            win.set_anchor(e, true);
        }
        win.set_exclusive_zone(-1);
        win.set_keyboard_mode(KeyboardMode::Exclusive);
        win.set_decorated(false);
        win.add_css_class("hs-layer");

        let area = DrawArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_cursor_from_name(Some("crosshair"));
        win.set_child(Some(&area));

        let tex = img.texture();
        let ov = Rc::new(Ov {
            win,
            area,
            monitor: monitor.clone(),
            img,
            tex,
            st: RefCell::new(State { sel: None, mode: Mode::Idle, press: (0.0, 0.0), start: None, mouse: None, last_click: None }),
            picker: Rc::downgrade(picker),
        });
        ov.connect();
        ov
    }

    fn connect(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.area.set_draw(move |w, sn, width, height| {
            if let Some(ov) = weak.upgrade() {
                ov.draw(w, sn, width, height);
            }
        });

        let drag = gtk::GestureDrag::new();
        drag.set_button(1);
        let w = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            if let Some(ov) = w.upgrade() {
                ov.press(x as f32, y as f32);
            }
        });
        let w = Rc::downgrade(self);
        drag.connect_drag_update(move |g, dx, dy| {
            if let (Some(ov), Some((sx, sy))) = (w.upgrade(), g.start_point()) {
                ov.drag((sx + dx) as f32, (sy + dy) as f32);
            }
        });
        let w = Rc::downgrade(self);
        drag.connect_drag_end(move |g, dx, dy| {
            if let (Some(ov), Some((sx, sy))) = (w.upgrade(), g.start_point()) {
                ov.release((sx + dx) as f32, (sy + dy) as f32);
            }
        });
        self.area.add_controller(drag);

        let right = gtk::GestureClick::new();
        right.set_button(3);
        let w = Rc::downgrade(self);
        right.connect_pressed(move |_, _, _, _| {
            if let Some(ov) = w.upgrade() {
                // Right-click drops the box; with no box it cancels.
                let had = ov.st.borrow_mut().sel.take().is_some();
                if had {
                    ov.st.borrow_mut().mode = Mode::Idle;
                    ov.area.queue_draw();
                } else if let Some(p) = ov.picker.upgrade() {
                    p.finish(None);
                }
            }
        });
        self.area.add_controller(right);

        let motion = gtk::EventControllerMotion::new();
        let w = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            if let Some(ov) = w.upgrade() {
                ov.hover(x as f32, y as f32);
            }
        });
        let w = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(ov) = w.upgrade() {
                ov.st.borrow_mut().mouse = None;
                ov.area.queue_draw();
            }
        });
        self.area.add_controller(motion);

        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| match w.upgrade() {
            Some(ov) => ov.key(key, state),
            None => glib::Propagation::Proceed,
        });
        self.win.add_controller(keys);
    }

    // ---- geometry ----------------------------------------------------------
    /// Image pixels per logical pixel. When the screen's scale matches the
    /// capture's, draw at exactly 1 image px = 1 device px: Cairo then blits
    /// instead of resampling 5 MP (the logical size rounds, e.g. 2992 x 1.15 =
    /// 3440.8, which would otherwise force a filtered scale).
    fn ratio(&self) -> f32 {
        let surface = self.area.native().and_then(|n| n.surface()).map(|s| s.scale() as f32);
        let fit = self.img.w as f32 / self.area.width().max(1) as f32;
        match surface {
            Some(s) if (s - fit).abs() < 0.01 => s,
            _ => fit,
        }
    }

    fn size(&self) -> (f32, f32) {
        (self.area.width() as f32, self.area.height() as f32)
    }

    fn clamp(&self, x: f32, y: f32) -> (f32, f32) {
        let (w, h) = self.size();
        (x.clamp(0.0, w), y.clamp(0.0, h))
    }

    fn bounded(&self, r: R) -> R {
        let (w, h) = self.size();
        let dx = if r.x0 < 0.0 { -r.x0 } else if r.x1 > w { w - r.x1 } else { 0.0 };
        let dy = if r.y0 < 0.0 { -r.y0 } else if r.y1 > h { h - r.y1 } else { 0.0 };
        r.moved(dx, dy)
    }

    fn handle_at(sel: &R, x: f32, y: f32) -> Option<u8> {
        HANDLES.iter().find_map(|&(m, fx, fy)| {
            let (hx, hy) = (sel.x0 + sel.w() * fx, sel.y0 + sel.h() * fy);
            ((hx - x).abs() <= HANDLE_HIT && (hy - y).abs() <= HANDLE_HIT).then_some(m)
        })
    }

    fn buttons(&self, sel: &R) -> [(Btn, graphene::Rect); 2] {
        let (w, h) = self.size();
        let total = BTN_W * 2.0 + BTN_GAP;
        let x = (sel.x1 - total).clamp(4.0, (w - total - 4.0).max(4.0));
        let mut y = sel.y1 + 10.0;
        if y + BTN_H > h - 4.0 {
            y = sel.y0 - BTN_H - 10.0;
            if y < 4.0 {
                y = sel.y1 - BTN_H - 10.0;
            }
        }
        [(Btn::Cancel, rect(x, y, BTN_W, BTN_H)), (Btn::Capture, rect(x + BTN_W + BTN_GAP, y, BTN_W, BTN_H))]
    }

    fn button_at(&self, x: f32, y: f32) -> Option<Btn> {
        let st = self.st.borrow();
        let sel = st.sel?;
        if st.mode != Mode::Idle && !matches!(st.mode, Mode::Button(_)) {
            return None;
        }
        self.buttons(&sel).into_iter().find(|(_, r)| r.contains_point(&graphene::Point::new(x, y))).map(|(b, _)| b)
    }

    // ---- input ---------------------------------------------------------------
    fn press(self: &Rc<Self>, x: f32, y: f32) {
        if let Some(b) = self.button_at(x, y) {
            self.st.borrow_mut().mode = Mode::Button(b);
            return;
        }
        // double-click inside the box confirms
        let now = std::time::Instant::now();
        let dbl = {
            let st = self.st.borrow();
            let inside = st.sel.is_some_and(|s| s.contains(x, y));
            inside
                && st.last_click.is_some_and(|(t, lx, ly)| {
                    now.duration_since(t).as_millis() < 400 && (lx - x).abs() < 6.0 && (ly - y).abs() < 6.0
                })
        };
        if dbl {
            self.confirm();
            return;
        }
        let new_box = {
            let mut st = self.st.borrow_mut();
            st.last_click = Some((now, x, y));
            st.press = (x, y);
            st.start = st.sel;
            if let Some(m) = st.sel.and_then(|s| Self::handle_at(&s, x, y)) {
                st.mode = Mode::Resize(m);
                false
            } else if st.sel.is_some_and(|s| s.contains(x, y)) {
                st.mode = Mode::Move;
                false
            } else {
                st.mode = Mode::New;
                st.sel = Some(R::from_pts(x, y, x, y));
                true
            }
        };
        if new_box {
            if let Some(p) = self.picker.upgrade() {
                p.clear_others(self);
            }
        }
        self.area.queue_draw();
    }

    fn drag(&self, x: f32, y: f32) {
        {
            let mut st = self.st.borrow_mut();
            st.mouse = Some((x, y));
            let (px, py) = st.press;
            match st.mode {
                Mode::New => {
                    let (cx, cy) = self.clamp(x, y);
                    st.sel = Some(R::from_pts(px, py, cx, cy));
                }
                Mode::Move => {
                    if let Some(s) = st.start {
                        st.sel = Some(self.bounded(s.moved(x - px, y - py)));
                    }
                }
                Mode::Resize(m) => {
                    if let Some(s) = st.start {
                        let (dx, dy) = (x - px, y - py);
                        let mut r = s;
                        if m & 1 != 0 {
                            r.x0 += dx;
                        }
                        if m & 4 != 0 {
                            r.x1 += dx;
                        }
                        if m & 2 != 0 {
                            r.y0 += dy;
                        }
                        if m & 8 != 0 {
                            r.y1 += dy;
                        }
                        let (ax, ay) = self.clamp(r.x0, r.y0);
                        let (bx, by) = self.clamp(r.x1, r.y1);
                        st.sel = Some(R::from_pts(ax, ay, bx, by));
                    }
                }
                _ => {}
            }
        }
        self.area.queue_draw();
    }

    fn release(self: &Rc<Self>, x: f32, y: f32) {
        {
            // only a click (not the end of a drag) can start a double-click
            let mut st = self.st.borrow_mut();
            if (st.press.0 - x).abs() > 4.0 || (st.press.1 - y).abs() > 4.0 {
                st.last_click = None;
            }
        }
        let mode = std::mem::replace(&mut self.st.borrow_mut().mode, Mode::Idle);
        match mode {
            Mode::Button(b) => {
                if self.button_at(x, y) == Some(b) {
                    match b {
                        Btn::Capture => self.confirm(),
                        Btn::Cancel => {
                            if let Some(p) = self.picker.upgrade() {
                                p.finish(None)
                            }
                        }
                    }
                }
                return;
            }
            Mode::New => {
                let mut st = self.st.borrow_mut();
                if st.sel.is_some_and(|s| s.w() < MIN_SIZE && s.h() < MIN_SIZE) {
                    st.sel = st.start; // a stray click keeps the old box
                }
            }
            _ => {}
        }
        self.hover(x, y);
    }

    fn hover(&self, x: f32, y: f32) {
        self.st.borrow_mut().mouse = Some((x, y));
        let cursor = if self.button_at(x, y).is_some() {
            "pointer"
        } else {
            let st = self.st.borrow();
            match st.sel {
                Some(s) if st.mode == Mode::Idle => match Self::handle_at(&s, x, y) {
                    Some(m) => cursor_for(m),
                    None if s.contains(x, y) => "move",
                    None => "crosshair",
                },
                _ => match st.mode {
                    Mode::Move => "move",
                    Mode::Resize(m) => cursor_for(m),
                    _ => "crosshair",
                },
            }
        };
        self.area.set_cursor_from_name(Some(cursor));
        self.area.queue_draw();
    }

    fn key(self: &Rc<Self>, key: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        use gdk::Key;
        let picker = self.picker.upgrade();
        match key {
            Key::Escape => {
                if let Some(p) = picker {
                    p.finish(None);
                }
            }
            Key::Return | Key::KP_Enter | Key::space => {
                if let Some(p) = picker {
                    p.confirm_any(self);
                }
            }
            Key::Left | Key::Right | Key::Up | Key::Down => {
                let step = if state.contains(gdk::ModifierType::SHIFT_MASK) { 10.0 } else { 1.0 };
                let (dx, dy) = match key {
                    Key::Left => (-step, 0.0),
                    Key::Right => (step, 0.0),
                    Key::Up => (0.0, -step),
                    _ => (0.0, step),
                };
                let (w, h) = self.size();
                let mut st = self.st.borrow_mut();
                if let Some(s) = st.sel {
                    st.sel = Some(if state.contains(gdk::ModifierType::ALT_MASK) {
                        // Alt+arrows resize from the bottom-right corner
                        R { x1: (s.x1 + dx).clamp(s.x0 + MIN_SIZE, w), y1: (s.y1 + dy).clamp(s.y0 + MIN_SIZE, h), ..s }
                    } else {
                        self.bounded(s.moved(dx, dy))
                    });
                }
                drop(st);
                self.area.queue_draw();
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    fn confirm(&self) {
        let sel = self.st.borrow().sel;
        let img = match sel {
            None => self.img.crop(0, 0, self.img.w, self.img.h),
            Some(s) => {
                let r = self.ratio();
                let x = ((s.x0 * r).round() as usize).min(self.img.w - 1);
                let y = ((s.y0 * r).round() as usize).min(self.img.h - 1);
                let w = ((s.w() * r).round() as usize).clamp(1, self.img.w - x);
                let h = ((s.h() * r).round() as usize).clamp(1, self.img.h - y);
                self.img.crop(x, y, w, h)
            }
        };
        if let Some(p) = self.picker.upgrade() {
            p.finish(Some((img, self.monitor.clone())));
        }
    }

    // ---- drawing ---------------------------------------------------------------
    fn draw(&self, widget: &DrawArea, sn: &gtk::Snapshot, w: f32, h: f32) {
        let st = self.st.borrow();
        let r = self.ratio();
        let (tw, th) = (self.img.w as f32 / r, self.img.h as f32 / r);
        if tw < w || th < h {
            sn.append_color(&gdk::RGBA::BLACK, &rect(0.0, 0.0, w, h)); // sub-pixel sliver
        }
        sn.append_texture(&self.tex, &rect(0.0, 0.0, tw, th));
        let dim = rgba(0.0, 0.0, 0.0, 0.47);
        let Some(s) = st.sel else {
            sn.append_color(&dim, &rect(0.0, 0.0, w, h));
            let l = util::layout(widget, "Drag to select an area   ·   Enter = whole screen   ·   Esc = cancel", 11.0, false);
            let (lw, lh) = l.pixel_size();
            let (pw, ph) = (lw as f32 + 32.0, lh as f32 + 18.0);
            util::pill(sn, &l, (w - pw) / 2.0, 36.0, 16.0, 9.0, ph / 2.0);
            return;
        };
        // dim everything but the box (four rects around it)
        sn.append_color(&dim, &rect(0.0, 0.0, w, s.y0));
        sn.append_color(&dim, &rect(0.0, s.y1, w, h - s.y1));
        sn.append_color(&dim, &rect(0.0, s.y0, s.x0, s.h()));
        sn.append_color(&dim, &rect(s.x1, s.y0, w - s.x1, s.h()));

        let white = rgba(1.0, 1.0, 1.0, 0.92);
        sn.append_border(&util::rounded(s.g(), 0.0), &[1.0; 4], &[white; 4]);

        if st.mode != Mode::New || s.w() > 40.0 {
            let edge = rgba(0.0, 0.0, 0.0, 0.35);
            for (_, fx, fy) in HANDLES {
                let (hx, hy) = (s.x0 + s.w() * fx, s.y0 + s.h() * fy);
                let r = rect(hx - HANDLE, hy - HANDLE, 2.0 * HANDLE, 2.0 * HANDLE);
                sn.append_color(&gdk::RGBA::WHITE, &r);
                sn.append_border(&util::rounded(r, 0.0), &[1.0; 4], &[edge; 4]);
            }
        }

        let ratio = self.ratio();
        let label = format!("{} × {}", (s.w() * ratio).round(), (s.h() * ratio).round());
        let l = util::layout(widget, &label, 10.0, true);
        let ly = if s.y0 > 34.0 { s.y0 - 30.0 } else { s.y0 + 8.0 };
        util::pill(sn, &l, s.x0.max(4.0), ly, 8.0, 4.0, 6.0);

        if st.mode == Mode::Idle || matches!(st.mode, Mode::Button(_)) {
            let hover = st.mouse.and_then(|(mx, my)| {
                self.buttons(&s).into_iter().find(|(_, r)| r.contains_point(&graphene::Point::new(mx, my))).map(|(b, _)| b)
            });
            for (b, r) in self.buttons(&s) {
                let hot = hover == Some(b);
                let (bg, text) = match b {
                    Btn::Capture => (if hot { rgba(0.24, 0.63, 1.0, 1.0) } else { rgba(0.04, 0.52, 1.0, 1.0) }, "✓  Capture"),
                    Btn::Cancel => (if hot { rgba(0.27, 0.27, 0.27, 0.96) } else { rgba(0.16, 0.16, 0.16, 0.92) }, "✕  Cancel"),
                };
                let rr = util::rounded(r, 7.0);
                sn.push_rounded_clip(&rr);
                sn.append_color(&bg, &r);
                sn.pop();
                sn.append_border(&rr, &[1.0; 4], &[rgba(1.0, 1.0, 1.0, 0.22); 4]);
                let l = util::layout(widget, text, 10.0, true);
                let (lw, lh) = l.pixel_size();
                sn.save();
                sn.translate(&graphene::Point::new(r.x() + (r.width() - lw as f32) / 2.0, r.y() + (r.height() - lh as f32) / 2.0));
                sn.append_layout(&l, &gdk::RGBA::WHITE);
                sn.restore();
            }
        }
    }
}

/// `holyshot __selftest-area`: drives the picker through the same handlers the
/// mouse uses (draw, resize, move, stray click, Capture button).
pub fn selftest(picker: &Rc<Picker>) {
    let p = picker.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
        let Some(ov) = p.ovs.borrow().first().cloned() else { return };
        ov.press(400.0, 300.0);
        ov.drag(700.0, 500.0);
        ov.drag(900.0, 600.0);
        ov.release(900.0, 600.0);
        println!("drawn   {:?}", ov.st.borrow().sel);
        ov.press(400.0, 300.0); // top-left handle
        ov.drag(300.0, 250.0);
        ov.release(300.0, 250.0);
        println!("resized {:?}", ov.st.borrow().sel);
        ov.press(600.0, 450.0); // inside: move
        ov.drag(700.0, 500.0);
        ov.release(700.0, 500.0);
        println!("moved   {:?}", ov.st.borrow().sel);
        ov.press(1500.0, 900.0); // stray click outside
        ov.release(1500.0, 900.0);
        println!("stray   {:?}", ov.st.borrow().sel);
        let sel = ov.st.borrow().sel.unwrap();
        let btn = ov.buttons(&sel)[1].1;
        println!("expect crop {}x{}", (sel.w() * ov.ratio()).round(), (sel.h() * ov.ratio()).round());
        let (bx, by) = (btn.x() + 5.0, btn.y() + 5.0);
        ov.hover(bx, by);
        ov.press(bx, by);
        ov.release(bx, by);
    });
}
