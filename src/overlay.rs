//! Full-screen region picker: one layer-shell overlay per monitor showing the
//! frozen, dimmed capture. Drag a box, then move/resize it at leisure;
//! confirm with Enter, double-click or the Capture button (or R / the Record
//! button to record it instead). Space switches to window mode (as on a Mac):
//! the window under the mouse lights up, click it.

use gtk::{gdk, glib, graphene, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use crate::capture::Win;
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

/// What the pick is for.
#[derive(Clone, Copy, PartialEq)]
pub enum Purpose {
    Shot,
    Record,
}

pub enum Picked {
    /// `img`: the frozen pixels of the box. `rect`: the box in global
    /// logical coordinates.
    Area { img: Img, mon: gdk::Monitor, rect: (i32, i32, i32, i32) },
    /// `img`: the frozen pixels of the window's visible part.
    Window { win: Win, img: Img, mon: gdk::Monitor },
}

#[derive(Clone, Copy, PartialEq)]
enum Btn {
    Timer,
    Capture,
    Record,
    Cancel,
}

struct State {
    sel: Option<R>,
    mode: Mode,
    press: (f32, f32),
    start: Option<R>,
    mouse: Option<(f32, f32)>,
    last_click: Option<(std::time::Instant, f32, f32)>,
    /// Window mode: index into `Picker::windows` under the mouse.
    hot: Option<usize>,
    /// The box we opened with (last time's); a hint shows while it's untouched.
    restored: Option<R>,
}

struct Ov {
    win: gtk::Window,
    area: DrawArea,
    monitor: gdk::Monitor,
    img: Img,
    tex: gdk::Texture,
    /// The monitor's top-left in global logical coordinates.
    origin: (i32, i32),
    st: RefCell<State>,
    picker: Weak<Picker>,
}

pub struct Picker {
    hs: Weak<Hs>,
    ovs: RefCell<Vec<Rc<Ov>>>,
    finished: Cell<bool>,
    /// What Enter / a click / double-click does.
    pub purpose: Purpose,
    /// Offer Record as well (not while a recording is already running).
    can_record: bool,
    /// Topmost first, global logical coordinates.
    windows: Vec<Win>,
    window_mode: Cell<bool>,
    /// Self-timer in seconds (0 = off); T or the ⏱ button cycles it.
    timer: Cell<u32>,
}

const TIMERS: [u32; 4] = [0, 3, 5, 10];

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
    pub fn start(hs: &Rc<Hs>, img: Img, windows: Vec<Win>, purpose: Purpose, window_mode: bool) -> Rc<Picker> {
        let picker = Rc::new(Picker {
            hs: Rc::downgrade(hs),
            ovs: RefCell::default(),
            finished: Cell::new(false),
            purpose,
            can_record: purpose == Purpose::Record || hs.recorder.borrow().is_none(),
            windows,
            window_mode: Cell::new(window_mode),
            timer: Cell::new(0),
        });
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
        picker.restore_last_area();
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

    /// Open with last time's box, on the monitor that holds it.
    fn restore_last_area(&self) {
        let Some((x, y, w, h)) = util::last_area() else { return };
        for ov in self.ovs.borrow().iter() {
            let g = ov.monitor.geometry();
            let (lx, ly) = (x - g.x(), y - g.y());
            if lx >= 0 && ly >= 0 && lx + w <= g.width() && ly + h <= g.height() {
                let r = R { x0: lx as f32, y0: ly as f32, x1: (lx + w) as f32, y1: (ly + h) as f32 };
                let mut st = ov.st.borrow_mut();
                st.sel = Some(r);
                st.restored = Some(r);
                return;
            }
        }
    }

    fn cycle_timer(&self) {
        let i = TIMERS.iter().position(|&t| t == self.timer.get()).unwrap_or(0);
        self.timer.set(TIMERS[(i + 1) % TIMERS.len()]);
        self.redraw();
    }

    fn clear_box(&self) {
        for ov in self.ovs.borrow().iter() {
            ov.st.borrow_mut().sel = None;
        }
        self.redraw();
    }

    fn redraw(&self) {
        for ov in self.ovs.borrow().iter() {
            ov.area.queue_draw();
        }
    }

    /// Close without capturing (shortcut pressed again).
    pub fn close(&self) {
        self.finish(None, self.purpose);
    }

    fn finish(&self, result: Option<Picked>, purpose: Purpose) {
        if self.finished.replace(true) {
            return;
        }
        let ovs = std::mem::take(&mut *self.ovs.borrow_mut());
        if let Some(hs) = self.hs.upgrade() {
            // Show the preview first so the app always has a window.
            hs.picked(result, purpose, self.timer.get());
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

    fn toggle_window_mode(&self) {
        self.window_mode.set(!self.window_mode.get());
        for ov in self.ovs.borrow().iter() {
            let mouse = {
                let mut st = ov.st.borrow_mut();
                st.mode = Mode::Idle;
                st.hot = None;
                st.mouse
            };
            match mouse {
                Some((x, y)) => ov.hover(x, y),
                None => ov.area.queue_draw(),
            }
        }
    }

    fn confirm_any(&self, focused: &Ov, purpose: Purpose) {
        if self.window_mode.get() {
            let ovs = self.ovs.borrow().clone();
            if let Some(o) = ovs.iter().find(|o| o.st.borrow().hot.is_some()) {
                o.confirm_window(purpose);
            }
            return;
        }
        // Enter goes to whichever overlay has keyboard focus, which may not be
        // where the box is: capture the box, else the screen under the mouse.
        let ovs = self.ovs.borrow().clone();
        let target = ovs
            .iter()
            .find(|o| o.st.borrow().sel.is_some())
            .or_else(|| ovs.iter().find(|o| o.st.borrow().mouse.is_some()))
            .cloned();
        match target {
            Some(o) => o.confirm(purpose),
            None => focused.confirm(purpose),
        }
    }
}

impl Ov {
    fn new(hs: &Rc<Hs>, picker: &Rc<Picker>, monitor: &gdk::Monitor, img: Img) -> Rc<Ov> {
        let win = gtk::Window::new();
        win.set_application(Some(&hs.app));
        win.init_layer_shell();
        win.set_layer(Layer::Overlay);
        win.set_namespace(Some(util::LAYER_NAMESPACE));
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
        let g = monitor.geometry();
        let ov = Rc::new(Ov {
            win,
            area,
            monitor: monitor.clone(),
            img,
            tex,
            origin: (g.x(), g.y()),
            st: RefCell::new(State {
                sel: None,
                mode: Mode::Idle,
                press: (0.0, 0.0),
                start: None,
                mouse: None,
                last_click: None,
                hot: None,
                restored: None,
            }),
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
                let window_mode = ov.picker.upgrade().is_some_and(|p| p.window_mode.get());
                let had = !window_mode && ov.st.borrow_mut().sel.take().is_some();
                if had {
                    ov.st.borrow_mut().mode = Mode::Idle;
                    ov.area.queue_draw();
                } else if let Some(p) = ov.picker.upgrade() {
                    p.finish(None, p.purpose);
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

    fn purpose(&self) -> Purpose {
        self.picker.upgrade().map_or(Purpose::Shot, |p| p.purpose)
    }

    fn window_mode(&self) -> bool {
        self.picker.upgrade().is_some_and(|p| p.window_mode.get())
    }

    /// Window `i` in this overlay's coordinates (may extend past the edges).
    fn window_rect(&self, win: &Win) -> R {
        let (x, y) = ((win.x - self.origin.0) as f32, (win.y - self.origin.1) as f32);
        R { x0: x, y0: y, x1: x + win.w as f32, y1: y + win.h as f32 }
    }

    /// The topmost window under (x, y).
    fn window_at(&self, x: f32, y: f32) -> Option<usize> {
        let p = self.picker.upgrade()?;
        p.windows.iter().position(|w| self.window_rect(w).contains(x, y))
    }

    /// Image pixels for a rect in overlay coordinates, clipped to the screen.
    fn pixels(&self, s: R) -> Img {
        let r = self.ratio();
        let (w, h) = self.size();
        let s = R::from_pts(s.x0.clamp(0.0, w), s.y0.clamp(0.0, h), s.x1.clamp(0.0, w), s.y1.clamp(0.0, h));
        let x = ((s.x0 * r).round() as usize).min(self.img.w - 1);
        let y = ((s.y0 * r).round() as usize).min(self.img.h - 1);
        let w = ((s.w() * r).round() as usize).clamp(1, self.img.w - x);
        let h = ((s.h() * r).round() as usize).clamp(1, self.img.h - y);
        self.img.crop(x, y, w, h)
    }

    /// Left to right, the default action last. Shot: Cancel, Record, Capture.
    fn buttons(&self, sel: &R) -> Vec<(Btn, graphene::Rect)> {
        let btns: &[Btn] = match self.picker.upgrade().map(|p| (p.purpose, p.can_record)) {
            Some((Purpose::Record, _)) => &[Btn::Timer, Btn::Cancel, Btn::Record],
            Some((Purpose::Shot, true)) => &[Btn::Timer, Btn::Cancel, Btn::Record, Btn::Capture],
            _ => &[Btn::Timer, Btn::Cancel, Btn::Capture],
        };
        let (w, h) = self.size();
        let n = btns.len() as f32;
        let total = BTN_W * n + BTN_GAP * (n - 1.0);
        let x = (sel.x1 - total).clamp(4.0, (w - total - 4.0).max(4.0));
        let mut y = sel.y1 + 10.0;
        if y + BTN_H > h - 4.0 {
            y = sel.y0 - BTN_H - 10.0;
            if y < 4.0 {
                y = sel.y1 - BTN_H - 10.0;
            }
        }
        btns.iter().enumerate().map(|(i, &b)| (b, rect(x + i as f32 * (BTN_W + BTN_GAP), y, BTN_W, BTN_H))).collect()
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
        if self.window_mode() {
            let mut st = self.st.borrow_mut();
            st.press = (x, y);
            st.mode = Mode::Idle;
            return;
        }
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
            self.confirm(self.purpose());
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
        if self.window_mode() {
            self.hover(x, y);
            if self.st.borrow().hot.is_some() {
                self.confirm_window(self.purpose());
            }
            return;
        }
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
                        Btn::Capture => self.confirm(Purpose::Shot),
                        Btn::Record => self.confirm(Purpose::Record),
                        Btn::Timer => {
                            if let Some(p) = self.picker.upgrade() {
                                p.cycle_timer()
                            }
                        }
                        Btn::Cancel => {
                            if let Some(p) = self.picker.upgrade() {
                                p.finish(None, p.purpose)
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
        if self.window_mode() {
            let hot = self.window_at(x, y);
            let changed = std::mem::replace(&mut self.st.borrow_mut().hot, hot) != hot;
            self.area.set_cursor_from_name(Some(if hot.is_some() { "pointer" } else { "default" }));
            if changed {
                self.area.queue_draw();
            }
            return;
        }
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
                    p.finish(None, p.purpose);
                }
            }
            Key::Return | Key::KP_Enter => {
                if let Some(p) = picker {
                    p.confirm_any(self, p.purpose);
                }
            }
            Key::t | Key::T => {
                if let Some(p) = picker {
                    p.cycle_timer();
                }
            }
            Key::Delete | Key::BackSpace => {
                if let Some(p) = picker {
                    p.clear_box();
                }
            }
            Key::r | Key::R => {
                if let Some(p) = picker.filter(|p| p.can_record) {
                    p.confirm_any(self, Purpose::Record);
                }
            }
            Key::space => {
                if let Some(p) = picker {
                    p.toggle_window_mode();
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

    fn confirm(&self, purpose: Purpose) {
        let (w, h) = self.size();
        let sel = self.st.borrow().sel;
        let s = sel.unwrap_or(R { x0: 0.0, y0: 0.0, x1: w, y1: h });
        let img = self.pixels(s);
        let (ox, oy) = self.origin;
        let rect = (ox + s.x0.round() as i32, oy + s.y0.round() as i32, s.w().round().max(1.0) as i32, s.h().round().max(1.0) as i32);
        if sel.is_some() {
            util::save_last_area(rect); // not "whole screen"
        }
        if let Some(p) = self.picker.upgrade() {
            p.finish(Some(Picked::Area { img, mon: self.monitor.clone(), rect }), purpose);
        }
    }

    fn confirm_window(&self, purpose: Purpose) {
        let Some(p) = self.picker.upgrade() else { return };
        let Some(win) = self.st.borrow().hot.and_then(|i| p.windows.get(i).cloned()) else { return };
        let img = self.pixels(self.window_rect(&win));
        p.finish(Some(Picked::Window { win, img, mon: self.monitor.clone() }), purpose);
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
        let (record, can_record, timer) =
            self.picker.upgrade().map_or((false, false, 0), |p| (p.purpose == Purpose::Record, p.can_record, p.timer.get()));
        let timer_hint = match timer {
            0 => "   ·   T = timer".to_string(),
            t => format!("   ·   ⏱ {t} s (T)"),
        };
        if self.window_mode() {
            // Mac-style: nothing dimmed, the window under the mouse tinted blue.
            let hot = st.hot.and_then(|i| self.picker.upgrade()?.windows.get(i).cloned());
            if let Some(win) = &hot {
                let r = self.window_rect(win).g();
                sn.append_color(&rgba(0.25, 0.55, 1.0, 0.30), &r);
                sn.append_border(&util::rounded(r, 0.0), &[2.0; 4], &[rgba(0.25, 0.55, 1.0, 0.9); 4]);
            }
            let text = match (hot.is_some() || st.mouse.is_none(), record, can_record) {
                (true, false, true) => "Click a window to capture it   ·   R = record it   ·   Space = area   ·   Esc = cancel",
                (true, false, false) => "Click a window to capture it   ·   Space = area   ·   Esc = cancel",
                (true, true, _) => "Click a window to record it   ·   Space = area   ·   Esc = cancel",
                (false, ..) => "Move the mouse over a window   ·   Space = area   ·   Esc = cancel",
            };
            let text = text.replace("   ·   Esc", &format!("{timer_hint}   ·   Esc"));
            let l = util::layout(widget, &text, 11.0, false);
            let (lw, lh) = l.pixel_size();
            let (pw, ph) = (lw as f32 + 32.0, lh as f32 + 18.0);
            util::pill(sn, &l, (w - pw) / 2.0, 36.0, 16.0, 9.0, ph / 2.0);
            return;
        }
        let dim = rgba(0.0, 0.0, 0.0, 0.47);
        let Some(s) = st.sel else {
            sn.append_color(&dim, &rect(0.0, 0.0, w, h));
            let text = if record {
                "Drag an area to record   ·   Enter = whole screen   ·   Space = window   ·   Esc = cancel"
            } else {
                "Drag to select an area   ·   Enter = whole screen   ·   Space = window   ·   Esc = cancel"
            };
            let text = text.replace("   ·   Esc", &format!("{timer_hint}   ·   Esc"));
            let l = util::layout(widget, &text, 11.0, false);
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

        if st.restored == Some(s) && st.mode == Mode::Idle {
            let go = if record { "Enter = record it" } else { "Enter = capture it" };
            let text = format!("Last area   ·   {go}   ·   drag for a new one   ·   Delete = clear{timer_hint}");
            let l = util::layout(widget, &text, 11.0, false);
            let (lw, lh) = l.pixel_size();
            let (pw, ph) = (lw as f32 + 32.0, lh as f32 + 18.0);
            util::pill(sn, &l, (w - pw) / 2.0, 36.0, 16.0, 9.0, ph / 2.0);
        }

        if st.mode == Mode::Idle || matches!(st.mode, Mode::Button(_)) {
            let hover = st.mouse.and_then(|(mx, my)| {
                self.buttons(&s).into_iter().find(|(_, r)| r.contains_point(&graphene::Point::new(mx, my))).map(|(b, _)| b)
            });
            for (b, r) in self.buttons(&s) {
                let hot = hover == Some(b);
                let dark = if hot { rgba(0.27, 0.27, 0.27, 0.96) } else { rgba(0.16, 0.16, 0.16, 0.92) };
                let (bg, text): (_, String) = match b {
                    // red when it's the main action, dark beside Capture
                    Btn::Record if record => (if hot { rgba(1.0, 0.36, 0.33, 1.0) } else { rgba(0.9, 0.2, 0.2, 1.0) }, "●  Record".into()),
                    Btn::Record => (dark, "●  Record".into()),
                    Btn::Capture => (if hot { rgba(0.24, 0.63, 1.0, 1.0) } else { rgba(0.04, 0.52, 1.0, 1.0) }, "✓  Capture".into()),
                    Btn::Cancel => (dark, "✕  Cancel".into()),
                    Btn::Timer if timer == 0 => (dark, "⏱  Timer".into()),
                    // amber while set, so a forgotten timer is hard to miss
                    Btn::Timer => (if hot { rgba(0.95, 0.62, 0.15, 1.0) } else { rgba(0.85, 0.53, 0.08, 1.0) }, format!("⏱  {timer} s")),
                };
                let rr = util::rounded(r, 7.0);
                sn.push_rounded_clip(&rr);
                sn.append_color(&bg, &r);
                sn.pop();
                sn.append_border(&rr, &[1.0; 4], &[rgba(1.0, 1.0, 1.0, 0.22); 4]);
                let l = util::layout(widget, &text, 10.0, true);
                if b == Btn::Record && !record {
                    // red dot, white label
                    let attrs = gtk::pango::AttrList::new();
                    let mut red = gtk::pango::AttrColor::new_foreground(0xff00, 0x4400, 0x3c00);
                    red.set_end_index("●".len() as u32);
                    attrs.insert(red);
                    l.set_attributes(Some(&attrs));
                }
                let (lw, lh) = l.pixel_size();
                sn.save();
                sn.translate(&graphene::Point::new(r.x() + (r.width() - lw as f32) / 2.0, r.y() + (r.height() - lh as f32) / 2.0));
                sn.append_layout(&l, &gdk::RGBA::WHITE);
                sn.restore();
            }
        }
    }
}

/// `holyshot __selftest-window`: hovers the centre of the topmost window.
/// Renders what the overlay shows to `out` (PNG), then clicks.
pub fn hover_first_window(picker: &Rc<Picker>, out: &std::path::Path) -> bool {
    let Some(w) = picker.windows.first() else { return false };
    let ovs = picker.ovs.borrow().clone();
    for ov in ovs {
        let r = ov.window_rect(w);
        let (cx, cy) = ((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0);
        let (sw, sh) = ov.size();
        if cx >= 0.0 && cy >= 0.0 && cx < sw && cy < sh {
            ov.hover(cx, cy);
            let sn = gtk::Snapshot::new();
            ov.draw(&ov.area, &sn, sw, sh);
            if let (Some(node), Some(renderer)) = (sn.to_node(), ov.win.renderer()) {
                let _ = renderer.render_texture(node, None).save_to_png(out);
            }
            ov.press(cx, cy);
            ov.release(cx, cy); // click: capture it
            return true;
        }
    }
    false
}

/// `holyshot __selftest-timer OUT`: prints the restored box, sets the timer
/// to 3 s with the T key's handler, saves the overlay to `out`, presses Enter.
pub fn selftest_timer(picker: &Rc<Picker>, out: std::path::PathBuf) {
    let p = picker.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
        let Some(ov) = p.ovs.borrow().iter().find(|o| o.st.borrow().sel.is_some()).cloned().or_else(|| p.ovs.borrow().first().cloned())
        else {
            return;
        };
        println!("restored {:?}", ov.st.borrow().restored);
        p.cycle_timer();
        println!("timer {} s", p.timer.get());
        let (sw, sh) = ov.size();
        let sn = gtk::Snapshot::new();
        ov.draw(&ov.area, &sn, sw, sh);
        if let (Some(node), Some(renderer)) = (sn.to_node(), ov.win.renderer()) {
            let _ = renderer.render_texture(node, None).save_to_png(&out);
        }
        util::trace("selftest: Enter");
        p.confirm_any(&ov, p.purpose);
    });
}

/// `holyshot __selftest-area`: drives the picker through the same handlers the
/// mouse uses (draw, resize, move, stray click, Capture or Record button).
/// `shot`: also save what the overlay shows just before the click.
pub fn selftest(picker: &Rc<Picker>, record: bool, shot: Option<std::path::PathBuf>) {
    let p = picker.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
        p.clear_box(); // start from nothing, whatever was remembered
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
        let want = if record { Btn::Record } else { Btn::Capture };
        let btn = ov.buttons(&sel).into_iter().find(|(b, _)| *b == want).unwrap().1;
        println!("expect crop {}x{}", (sel.w() * ov.ratio()).round(), (sel.h() * ov.ratio()).round());
        let (bx, by) = (btn.x() + 5.0, btn.y() + 5.0);
        ov.hover(bx, by);
        if let Some(out) = &shot {
            let (sw, sh) = ov.size();
            let sn = gtk::Snapshot::new();
            ov.draw(&ov.area, &sn, sw, sh);
            if let (Some(node), Some(renderer)) = (sn.to_node(), ov.win.renderer()) {
                let _ = renderer.render_texture(node, None).save_to_png(out);
            }
        }
        ov.press(bx, by);
        ov.release(bx, by);
    });
}
