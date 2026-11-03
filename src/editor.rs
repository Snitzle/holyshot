//! The annotation window.

use gtk::{gdk, gio, glib, graphene, gsk, pango, prelude::*};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::draw::DrawArea;
use crate::img::Img;
use crate::shapes::{self, Ctx, Kind, Shape, Tool, P};
use crate::util::{self, rect, rgba};
use crate::Hs;

const TOOLS: [(Tool, &str, &str, &str, &str); 9] = [
    (Tool::Select, "Select", "tool-pointer-symbolic", "v", "Select, move and resize (V)"),
    (Tool::Arrow, "Arrow", "draw-arrow-symbolic", "a", "Arrow (A) – hold Shift to snap"),
    (Tool::Rect, "Box", "draw-rectangle-symbolic", "r", "Box (R) – hold Shift for a square"),
    (Tool::Ellipse, "Circle", "draw-ellipse-symbolic", "e", "Circle (E) – hold Shift for a perfect circle"),
    (Tool::Pen, "Draw", "draw-freehand-symbolic", "p", "Freehand pen (P) – hold Shift for a straight line"),
    (Tool::Highlight, "Highlight", "draw-highlight-symbolic", "h", "Highlighter (H) – hold Shift for a straight line"),
    (Tool::Text, "Text", "draw-text-symbolic", "t", "Text (T) – click to type, Enter to finish"),
    (Tool::Number, "Steps", "format-list-ordered-symbolic", "n", "Numbered steps (N) – each click adds the next number"),
    (Tool::Pixelate, "Pixelate", "pixelate-symbolic", "b", "Pixelate / hide something (B)"),
];

pub const COLORS: [(&str, &str); 8] = [
    ("Red", "#ff3b30"),
    ("Orange", "#ff9500"),
    ("Yellow", "#ffd60a"),
    ("Green", "#34c759"),
    ("Blue", "#0a84ff"),
    ("Purple", "#bf5af2"),
    ("Black", "#1c1c1e"),
    ("White", "#ffffff"),
];

const HANDLE_R: f32 = 5.0;
const TOL: f32 = 6.0; // widget px
const ACCENT: gdk::RGBA = gdk::RGBA::new(0.24, 0.68, 0.91, 1.0);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    Draw,
    Move,
    Handle(usize),
}

struct TextEdit {
    pos: P,
    editing: Option<usize>,
}

struct St {
    shapes: Vec<Shape>,
    tool: Tool,
    color: gdk::RGBA,
    colors: [gdk::RGBA; 2], // [everything else, highlighter]
    size: usize,
    selected: Option<usize>,
    mode: Mode,
    active: Option<Shape>,
    last: P,
    before: Option<Vec<Shape>>,
    moved: bool,
    undo: Vec<Vec<Shape>>,
    redo: Vec<Vec<Shape>>,
    text: Option<TextEdit>,
    path: Option<PathBuf>,
    dirty: bool,
    last_click: Option<(Instant, P)>,
    allow_close: bool,
}

struct Ui {
    tools: Vec<(Tool, gtk::ToggleButton)>,
    colors: Vec<(gdk::RGBA, gtk::ToggleButton)>,
    sizes: Vec<gtk::ToggleButton>,
    undo: gtk::Button,
    redo: gtk::Button,
    status: gtk::Label,
}

pub struct Editor {
    hs: Weak<Hs>,
    win: gtk::ApplicationWindow,
    canvas: DrawArea,
    /// One text box, shown/hidden and reused. Destroying a focused TextView
    /// crashes GTK 4.22's Wayland input-method code on the next IM event.
    tv: gtk::TextView,
    base: Img,
    base_tex: gdk::Texture,
    unit: f32,
    st: RefCell<St>,
    ui: RefCell<Option<Ui>>,
    syncing: Cell<bool>,
    text_css: gtk::CssProvider,
}

fn tool_of(s: &Shape) -> Tool {
    match s.kind {
        Kind::Arrow(..) => Tool::Arrow,
        Kind::Rect(..) => Tool::Rect,
        Kind::Ellipse(..) => Tool::Ellipse,
        Kind::Pixelate(..) => Tool::Pixelate,
        Kind::Pen { .. } => Tool::Pen,
        Kind::Highlight { .. } => Tool::Highlight,
        Kind::Text { .. } => Tool::Text,
        Kind::Number { .. } => Tool::Number,
    }
}

fn same_color(a: &gdk::RGBA, b: &gdk::RGBA) -> bool {
    (a.red() - b.red()).abs() < 0.004 && (a.green() - b.green()).abs() < 0.004 && (a.blue() - b.blue()).abs() < 0.004
}

/// `weak!(rc, (args...), ed => body)`: a signal handler holding only a weak
/// reference, so widgets never keep their editor alive.
macro_rules! weak {
    ($rc:expr, ($($a:ident : $t:ty),*), $ed:ident => $body:expr) => {{
        let w = Rc::downgrade($rc);
        move |$($a: $t),*| {
            if let Some($ed) = w.upgrade() {
                $body
            }
        }
    }};
}

impl Editor {
    pub fn open(hs: &Rc<Hs>, img: Img, path: Option<PathBuf>) -> Rc<Editor> {
        util::trace("editor: open");
        util::use_real_icon_theme();
        let win = gtk::ApplicationWindow::new(&hs.app);
        win.set_icon_name(Some("accessories-screenshot"));
        let canvas = DrawArea::new();
        canvas.set_hexpand(true);
        canvas.set_vexpand(true);
        canvas.set_focusable(true);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&canvas));
        win.set_child(Some(&overlay));

        let text_css = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(&WidgetExt::display(&win), &text_css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);

        let unit = img.scale.max(1.0) as f32;
        let base_tex = img.texture();
        let red = util::hex(COLORS[0].1);
        let tv = gtk::TextView::new();
        tv.add_css_class("hs-text");
        tv.set_halign(gtk::Align::Start);
        tv.set_valign(gtk::Align::Start);
        tv.set_accepts_tab(false);
        tv.set_visible(false);
        overlay.add_overlay(&tv);
        let ed = Rc::new(Editor {
            hs: Rc::downgrade(hs),
            win,
            canvas,

            tv,
            base: img,
            base_tex,
            unit,
            st: RefCell::new(St {
                shapes: vec![],
                tool: Tool::Arrow,
                color: red,
                colors: [red, util::hex(COLORS[2].1)],
                size: 2,
                selected: None,
                mode: Mode::Idle,
                active: None,
                last: P::default(),
                before: None,
                moved: false,
                undo: vec![],
                redo: vec![],
                text: None,
                path,
                dirty: false,
                last_click: None,
                allow_close: false,
            }),
            ui: RefCell::new(None),
            syncing: Cell::new(false),
            text_css,
        });
        ed.build_toolbar();
        ed.connect();
        ed.sync_toolbar();
        ed.update_title();
        ed.initial_size();
        ed.win.present();
        ed.canvas.grab_focus();
        ed
    }

    fn initial_size(&self) {
        let (lw, lh) = (self.base.w as f32 / self.unit, self.base.h as f32 / self.unit);
        let (mut w, mut h) = (lw + 80.0, lh + 150.0);
        if let Some(m) = util::monitors().first() {
            let g = m.geometry();
            w = w.min(g.width() as f32 * 0.9);
            h = h.min(g.height() as f32 * 0.9);
        }
        self.win.set_default_size(w.max(1180.0) as i32, h.max(560.0) as i32);
    }

    // ---- toolbar ---------------------------------------------------------------
    fn build_toolbar(self: &Rc<Self>) {
        let bar = gtk::HeaderBar::new();
        bar.set_title_widget(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
        let left = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        left.add_css_class("hs-bar");

        let mut tools = vec![];
        let mut group: Option<gtk::ToggleButton> = None;
        for (tool, label, icon, _, tip) in TOOLS {
            let b = gtk::ToggleButton::new();
            let col = gtk::Box::new(gtk::Orientation::Vertical, 1);
            let img = gtk::Image::from_icon_name(icon);
            img.set_pixel_size(22);
            col.append(&img);
            let l = gtk::Label::new(Some(label));
            l.add_css_class("hs-toollabel");
            col.append(&l);
            b.set_child(Some(&col));
            b.set_tooltip_text(Some(tip));
            b.add_css_class("flat");
            b.add_css_class("hs-tool");
            b.set_focusable(false);
            if let Some(g) = &group {
                b.set_group(Some(g));
            } else {
                group = Some(b.clone());
            }
            b.connect_toggled(weak!(self, (b: &gtk::ToggleButton), ed => {
                if b.is_active() && !ed.syncing.get() {
                    ed.set_tool(tool);
                }
            }));
            left.append(&b);
            tools.push((tool, b));
        }

        left.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        let mut colors = vec![];
        let mut cgroup: Option<gtk::ToggleButton> = None;
        for (i, (name, hexs)) in COLORS.iter().enumerate() {
            let b = gtk::ToggleButton::new();
            b.add_css_class("hs-swatch");
            b.add_css_class(&format!("hs-c{i}"));
            b.set_tooltip_text(Some(&format!("{name} ({})", i + 1)));
            b.set_focusable(false);
            b.set_valign(gtk::Align::Center);
            if let Some(g) = &cgroup {
                b.set_group(Some(g));
            } else {
                cgroup = Some(b.clone());
            }
            let c = util::hex(hexs);
            b.connect_toggled(weak!(self, (b: &gtk::ToggleButton), ed => {
                if b.is_active() && !ed.syncing.get() {
                    ed.set_color(c);
                }
            }));
            left.append(&b);
            colors.push((c, b));
        }

        left.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        let mut sizes = vec![];
        let mut sgroup: Option<gtk::ToggleButton> = None;
        for (ix, (label, r)) in [(1usize, ("Thin", 2.5f32)), (2, ("Medium", 4.5)), (3, ("Thick", 7.0))] {
            let b = gtk::ToggleButton::new();
            let dot = DrawArea::new();
            dot.set_size_request(22, 22);
            dot.set_draw(move |w, sn, width, height| {
                let pb = gsk::PathBuilder::new();
                pb.add_circle(&graphene::Point::new(width / 2.0, height / 2.0), r);
                sn.append_fill(&pb.to_path(), gsk::FillRule::Winding, &w.color());
            });
            b.set_child(Some(&dot));
            b.set_tooltip_text(Some(&format!("{label}  ([ and ] to change)")));
            b.add_css_class("flat");
            b.set_focusable(false);
            b.set_valign(gtk::Align::Center);
            if let Some(g) = &sgroup {
                b.set_group(Some(g));
            } else {
                sgroup = Some(b.clone());
            }
            b.connect_toggled(weak!(self, (b: &gtk::ToggleButton), ed => {
                if b.is_active() && !ed.syncing.get() {
                    ed.set_size(ix);
                }
            }));
            left.append(&b);
            sizes.push(b);
        }

        left.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        let undo = gtk::Button::from_icon_name("edit-undo-symbolic");
        undo.set_tooltip_text(Some("Undo (Ctrl+Z)"));
        undo.add_css_class("flat");
        undo.set_focusable(false);
        undo.set_valign(gtk::Align::Center);
        undo.connect_clicked(weak!(self, (_b: &gtk::Button), ed => ed.undo()));
        let redo = gtk::Button::from_icon_name("edit-redo-symbolic");
        redo.set_tooltip_text(Some("Redo (Ctrl+Shift+Z)"));
        redo.add_css_class("flat");
        redo.set_focusable(false);
        redo.set_valign(gtk::Align::Center);
        redo.connect_clicked(weak!(self, (_b: &gtk::Button), ed => ed.redo()));
        left.append(&undo);
        left.append(&redo);
        bar.pack_start(&left);

        let mk = |icon: &str, label: &str, tip: &str| {
            let b = gtk::Button::new();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.append(&gtk::Image::from_icon_name(icon));
            row.append(&gtk::Label::new(Some(label)));
            b.set_child(Some(&row));
            b.set_tooltip_text(Some(tip));
            b.set_focusable(false);
            b.set_valign(gtk::Align::Center);
            b
        };
        let save = mk("document-save-symbolic", "Save", "Save and close (Ctrl+S saves without closing)");
        save.add_css_class("suggested-action");
        save.connect_clicked(weak!(self, (_b: &gtk::Button), ed => ed.save(true)));
        let save_as = mk("document-save-as-symbolic", "Save As…", "Save a copy (Ctrl+Shift+S)");
        save_as.add_css_class("flat");
        save_as.connect_clicked(weak!(self, (_b: &gtk::Button), ed => ed.save_as()));
        let copy = mk("edit-copy-symbolic", "Copy", "Copy to clipboard (Ctrl+C)");
        copy.add_css_class("flat");
        copy.connect_clicked(weak!(self, (_b: &gtk::Button), ed => ed.copy()));
        let status = gtk::Label::new(None);
        status.add_css_class("dim-label");
        status.set_margin_end(8);
        bar.pack_end(&save);
        bar.pack_end(&save_as);
        bar.pack_end(&copy);
        bar.pack_end(&status);
        self.win.set_titlebar(Some(&bar));

        *self.ui.borrow_mut() = Some(Ui { tools, colors, sizes, undo, redo, status });
    }

    /// Reflect current tool/colour/size in the toolbar without triggering handlers.
    fn sync_toolbar(&self) {
        let (tool, color, size, can_undo, can_redo) = {
            let st = self.st.borrow();
            (st.tool, st.color, st.size, !st.undo.is_empty(), !st.redo.is_empty())
        };
        self.syncing.set(true);
        if let Some(ui) = self.ui.borrow().as_ref() {
            for (t, b) in &ui.tools {
                if *t == tool {
                    b.set_active(true);
                }
            }
            let mut any = false;
            for (c, b) in &ui.colors {
                if same_color(c, &color) {
                    b.set_active(true);
                    any = true;
                }
            }
            if !any {
                for (_, b) in &ui.colors {
                    b.set_active(false);
                }
            }
            ui.sizes[size - 1].set_active(true);
            ui.undo.set_sensitive(can_undo);
            ui.redo.set_sensitive(can_redo);
        }
        self.syncing.set(false);
    }

    fn flash(self: &Rc<Self>, msg: &str) {
        let Some(label) = self.ui.borrow().as_ref().map(|u| u.status.clone()) else { return };
        label.set_text(msg);
        let m = msg.to_string();
        glib::timeout_add_local_once(Duration::from_millis(2500), move || {
            if label.text() == m {
                label.set_text("");
            }
        });
    }

    fn update_title(&self) {
        let st = self.st.borrow();
        let name = st.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        let name = name.unwrap_or_else(|| "Screenshot".into());
        self.win.set_title(Some(&format!("{}{name} — HolyShot", if st.dirty { "● " } else { "" })));
    }

    // ---- events ----------------------------------------------------------------
    fn connect(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.canvas.set_draw(move |_, sn, width, height| {
            if let Some(ed) = w.upgrade() {
                ed.draw(sn, width, height);
            }
        });

        let drag = gtk::GestureDrag::new();
        drag.set_button(1);
        drag.connect_drag_begin(weak!(self, (g: &gtk::GestureDrag, x: f64, y: f64), ed => {
            let shift = g.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            ed.press(x as f32, y as f32, shift)
        }));
        drag.connect_drag_update(weak!(self, (g: &gtk::GestureDrag, dx: f64, dy: f64), ed => {
            if let Some((sx, sy)) = g.start_point() {
                let shift = g.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
                ed.drag((sx + dx) as f32, (sy + dy) as f32, shift)
            }
        }));
        drag.connect_drag_end(weak!(self, (g: &gtk::GestureDrag, dx: f64, dy: f64), ed => {
            if let Some((sx, sy)) = g.start_point() {
                ed.release((sx + dx) as f32, (sy + dy) as f32)
            }
        }));
        self.canvas.add_controller(drag);

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(weak!(self, (_m: &gtk::EventControllerMotion, x: f64, y: f64), ed => ed.update_cursor(Some((x as f32, y as f32)))));
        self.canvas.add_controller(motion);

        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| match w.upgrade() {
            Some(ed) => ed.key(key, state),
            None => glib::Propagation::Proceed,
        });
        self.win.add_controller(keys);

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let commit = key == gdk::Key::Escape
                || (matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) && !state.contains(gdk::ModifierType::SHIFT_MASK));
            if commit {
                if let Some(ed) = w.upgrade() {
                    glib::idle_add_local_once(move || ed.finish_text());
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.tv.add_controller(keys);
        let focus = gtk::EventControllerFocus::new();
        let w = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(ed) = w.upgrade() {
                glib::idle_add_local_once(move || ed.finish_text());
            }
        });
        self.tv.add_controller(focus);

        let w = Rc::downgrade(self);
        self.win.connect_close_request(move |_| match w.upgrade() {
            Some(ed) => ed.close_request(),
            None => glib::Propagation::Proceed,
        });
        let w = Rc::downgrade(self);
        self.win.connect_destroy(move |_| {
            if let Some(ed) = w.upgrade() {
                gtk::style_context_remove_provider_for_display(&WidgetExt::display(&ed.win), &ed.text_css);
                if let Some(hs) = ed.hs.upgrade() {
                    hs.editor_closed(&ed);
                }
            }
        });
    }

    fn key(self: &Rc<Self>, key: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        use gdk::Key;
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let lower = key.to_lower();
        if ctrl {
            match lower {
                Key::z if shift => self.redo(),
                Key::z => self.undo(),
                Key::y => self.redo(),
                Key::s if shift => self.save_as(),
                Key::s => self.save(false),
                Key::c => self.copy(),
                Key::w => self.win.close(),
                _ => return glib::Propagation::Proceed,
            }
            return glib::Propagation::Stop;
        }
        if self.st.borrow().text.is_some() {
            return glib::Propagation::Proceed;
        }
        if let Some(name) = lower.name() {
            if let Some(&(tool, ..)) = TOOLS.iter().find(|t| t.3 == name.as_str()) {
                self.set_tool(tool);
                self.sync_toolbar();
                return glib::Propagation::Stop;
            }
        }
        if let Some(d) = key.to_unicode().and_then(|c| c.to_digit(10)) {
            if (1..=8).contains(&d) {
                self.set_color(util::hex(COLORS[d as usize - 1].1));
                self.sync_toolbar();
                return glib::Propagation::Stop;
            }
        }
        match key {
            Key::bracketleft => {
                let s = self.st.borrow().size;
                self.set_size(s.saturating_sub(1).max(1));
                self.sync_toolbar();
            }
            Key::bracketright => {
                let s = self.st.borrow().size;
                self.set_size((s + 1).min(3));
                self.sync_toolbar();
            }
            Key::Delete | Key::BackSpace => self.delete_selected(),
            Key::Escape => self.select(None),
            Key::Left | Key::Right | Key::Up | Key::Down => {
                let step = if shift { 10.0 } else { 1.0 };
                let d = match key {
                    Key::Left => P::new(-step, 0.0),
                    Key::Right => P::new(step, 0.0),
                    Key::Up => P::new(0.0, -step),
                    _ => P::new(0.0, step),
                };
                let sel = self.st.borrow().selected;
                if let Some(i) = sel {
                    let snap = self.snapshot();
                    self.st.borrow_mut().shapes[i].translate(d);
                    self.push_undo(snap);
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    // ---- view geometry ----------------------------------------------------------
    fn fit(&self) -> (f32, P) {
        let (w, h) = (self.canvas.width() as f32, self.canvas.height() as f32);
        let (bw, bh) = (self.base.w as f32, self.base.h as f32);
        let margin = 28.0;
        let dpr = self.canvas.native().and_then(|n| n.surface()).map(|s| s.scale() as f32).unwrap_or(1.0);
        // never upscale past 1 image px = 1 screen px
        let s = ((w - 2.0 * margin).max(1.0) / bw).min((h - 2.0 * margin).max(1.0) / bh).min(1.0 / dpr);
        (s, P::new(((w - bw * s) / 2.0).round(), ((h - bh * s) / 2.0).round()))
    }

    fn to_img(&self, x: f32, y: f32) -> P {
        let (s, o) = self.fit();
        P::new((x - o.x) / s, (y - o.y) / s)
    }

    fn to_widget(&self, p: P) -> P {
        let (s, o) = self.fit();
        P::new(p.x * s + o.x, p.y * s + o.y)
    }

    fn tol(&self) -> f32 {
        TOL / self.fit().0
    }

    fn ctx(&self) -> (pango::Context, &Img, &gdk::Texture) {
        (self.canvas.pango_context(), &self.base, &self.base_tex)
    }

    // ---- drawing -------------------------------------------------------------------
    fn draw(&self, sn: &gtk::Snapshot, w: f32, h: f32) {
        let (s, o) = self.fit();
        let st = self.st.borrow();
        sn.append_color(&rgba(0.0, 0.0, 0.0, 0.22), &rect(0.0, 0.0, w, h));
        let ir = rect(o.x, o.y, self.base.w as f32 * s, self.base.h as f32 * s);
        sn.append_outset_shadow(&util::rounded(ir, 0.0), &rgba(0.0, 0.0, 0.0, 0.45), 0.0, 3.0, 0.0, 16.0);
        let pctx = self.canvas.pango_context();
        let ctx = Ctx { base: &self.base, base_tex: &self.base_tex, pango: &pctx };
        sn.push_clip(&ir);
        sn.save();
        sn.translate(&graphene::Point::new(o.x, o.y));
        sn.scale(s, s);
        shapes::render(sn, &ctx, &st.shapes, st.active.as_ref(), gsk::ScalingFilter::Trilinear);
        sn.restore();
        sn.pop();

        if let Some(shape) = st.selected.and_then(|i| st.shapes.get(i)) {
            let handles = shape.handles();
            let dashed = gsk::Stroke::new(1.2);
            dashed.set_dash(&[5.0, 4.0]);
            if handles.is_empty() || handles.len() == 4 {
                let b = shape.bounds(&ctx);
                let (a, z) = (
                    self.to_widget(P::new(b.x(), b.y())),
                    self.to_widget(P::new(b.x() + b.width(), b.y() + b.height())),
                );
                let pb = gsk::PathBuilder::new();
                pb.add_rect(&rect(a.x - 2.0, a.y - 2.0, z.x - a.x + 4.0, z.y - a.y + 4.0));
                sn.append_stroke(&pb.to_path(), &dashed, &ACCENT);
            }
            for hp in handles {
                let c = self.to_widget(hp);
                let pb = gsk::PathBuilder::new();
                pb.add_circle(&graphene::Point::new(c.x, c.y), HANDLE_R);
                let path = pb.to_path();
                sn.append_fill(&path, gsk::FillRule::Winding, &ACCENT);
                sn.append_stroke(&path, &gsk::Stroke::new(1.5), &gdk::RGBA::WHITE);
            }
        }
    }

    // ---- model helpers ---------------------------------------------------------------
    fn snapshot(&self) -> Vec<Shape> {
        self.st.borrow().shapes.clone()
    }

    fn push_undo(&self, snap: Vec<Shape>) {
        {
            let mut st = self.st.borrow_mut();
            st.undo.push(snap);
            if st.undo.len() > 200 {
                st.undo.remove(0);
            }
            st.redo.clear();
            st.dirty = true;
        }
        self.update_title();
        self.sync_toolbar();
        self.canvas.queue_draw();
    }

    fn undo(self: &Rc<Self>) {
        self.finish_text();
        {
            let mut st = self.st.borrow_mut();
            let Some(prev) = st.undo.pop() else { return };
            let cur = std::mem::replace(&mut st.shapes, prev);
            st.redo.push(cur);
            st.selected = None;
            st.dirty = true;
        }
        self.after_history();
    }

    fn redo(self: &Rc<Self>) {
        self.finish_text();
        {
            let mut st = self.st.borrow_mut();
            let Some(next) = st.redo.pop() else { return };
            let cur = std::mem::replace(&mut st.shapes, next);
            st.undo.push(cur);
            st.selected = None;
            st.dirty = true;
        }
        self.after_history();
    }

    fn after_history(&self) {
        self.update_title();
        self.sync_toolbar();
        self.canvas.queue_draw();
    }

    fn select(&self, i: Option<usize>) {
        let style = {
            let mut st = self.st.borrow_mut();
            st.selected = i;
            // reflect the shape's style in the toolbar
            i.and_then(|i| st.shapes.get(i)).map(|s| (s.color, s.size))
        };
        if let Some((c, s)) = style {
            let mut st = self.st.borrow_mut();
            st.color = c;
            st.size = s;
        }
        self.sync_toolbar();
        self.canvas.queue_draw();
    }

    fn slot(tool: Tool) -> usize {
        usize::from(tool == Tool::Highlight)
    }

    fn set_tool(self: &Rc<Self>, tool: Tool) {
        self.finish_text();
        {
            let mut st = self.st.borrow_mut();
            st.tool = tool;
            if tool != Tool::Select && st.selected.and_then(|i| st.shapes.get(i)).is_some_and(|s| tool_of(s) != tool) {
                st.selected = None;
            }
            st.color = st.colors[Self::slot(tool)];
        }
        self.sync_toolbar();
        self.update_cursor(None);
        self.canvas.queue_draw();
    }

    fn set_color(self: &Rc<Self>, c: gdk::RGBA) {
        let snap = {
            let mut st = self.st.borrow_mut();
            st.color = c;
            if st.selected.is_none() || st.tool != Tool::Select {
                let slot = Self::slot(st.tool);
                st.colors[slot] = c;
            }
            match st.selected {
                Some(i) if !same_color(&st.shapes[i].color, &c) => {
                    let snap = st.shapes.clone();
                    st.shapes[i].color = c;
                    Some(snap)
                }
                _ => None,
            }
        };
        if let Some(s) = snap {
            self.push_undo(s);
        }
        self.restyle_text();
    }

    fn set_size(self: &Rc<Self>, size: usize) {
        let snap = {
            let mut st = self.st.borrow_mut();
            st.size = size;
            match st.selected {
                Some(i) if st.shapes[i].size != size => {
                    let snap = st.shapes.clone();
                    st.shapes[i].size = size;
                    Some(snap)
                }
                _ => None,
            }
        };
        if let Some(s) = snap {
            self.push_undo(s);
        }
        self.restyle_text();
    }

    fn delete_selected(&self) {
        let sel = self.st.borrow().selected;
        if let Some(i) = sel {
            let snap = self.snapshot();
            {
                let mut st = self.st.borrow_mut();
                st.shapes.remove(i);
                st.selected = None;
            }
            self.push_undo(snap);
        }
    }

    fn shape_at(&self, p: P) -> Option<usize> {
        let (pctx, base, tex) = self.ctx();
        let ctx = Ctx { base, base_tex: tex, pango: &pctx };
        let tol = self.tol();
        let st = self.st.borrow();
        st.shapes.iter().rposition(|s| s.hit(p, tol, &ctx))
    }

    fn selected_hit(&self, p: P) -> bool {
        let (pctx, base, tex) = self.ctx();
        let ctx = Ctx { base, base_tex: tex, pango: &pctx };
        let st = self.st.borrow();
        st.selected.and_then(|i| st.shapes.get(i)).is_some_and(|s| s.hit(p, self.tol(), &ctx))
    }

    fn handle_at(&self, x: f32, y: f32) -> Option<usize> {
        let st = self.st.borrow();
        let s = st.selected.and_then(|i| st.shapes.get(i))?;
        s.handles().into_iter().position(|h| {
            let c = self.to_widget(h);
            (c.x - x).powi(2) + (c.y - y).powi(2) <= (HANDLE_R + 4.0).powi(2)
        })
    }

    fn update_cursor(&self, pos: Option<(f32, f32)>) {
        let name = match pos {
            Some((x, y)) if self.handle_at(x, y).is_some() => "move",
            Some((x, y)) if self.selected_hit(self.to_img(x, y)) => "move",
            Some((x, y)) if self.st.borrow().tool == Tool::Select => {
                if self.shape_at(self.to_img(x, y)).is_some() { "pointer" } else { "default" }
            }
            _ => match self.st.borrow().tool {
                Tool::Select => "default",
                Tool::Text => "text",
                _ => "crosshair",
            },
        };
        self.canvas.set_cursor_from_name(Some(name));
    }

    // ---- mouse ------------------------------------------------------------------------
    fn press(self: &Rc<Self>, x: f32, y: f32, _shift: bool) {
        if self.st.borrow().text.is_some() {
            self.finish_text();
            return;
        }
        self.canvas.grab_focus();
        let p = self.to_img(x, y);
        let now = Instant::now();
        let dbl = self
            .st
            .borrow_mut()
            .last_click
            .replace((now, p))
            .is_some_and(|(t, lp)| now.duration_since(t).as_millis() < 400 && (lp - p).len() < 6.0 / self.fit().0);
        if dbl {
            if let Some(i) = self.shape_at(p) {
                if self.st.borrow().shapes[i].is_text() {
                    self.st.borrow_mut().mode = Mode::Idle;
                    self.start_text(p, Some(i));
                    return;
                }
            }
        }
        let snap = self.snapshot();
        {
            let mut st = self.st.borrow_mut();
            st.last = p;
            st.moved = false;
            st.before = Some(snap);
        }
        if let Some(h) = self.handle_at(x, y) {
            self.st.borrow_mut().mode = Mode::Handle(h);
            return;
        }
        if self.selected_hit(p) {
            self.st.borrow_mut().mode = Mode::Move;
            return;
        }
        let tool = self.st.borrow().tool;
        match tool {
            Tool::Select => {
                let hit = self.shape_at(p);
                self.select(hit);
                self.st.borrow_mut().mode = if hit.is_some() { Mode::Move } else { Mode::Idle };
                return;
            }
            Tool::Text => {
                let existing = self.shape_at(p).filter(|&i| self.st.borrow().shapes[i].is_text());
                self.start_text(p, existing);
                return;
            }
            _ => {}
        }
        self.select(None);
        let mut st = self.st.borrow_mut();
        let (c, s, u) = (st.color, st.size, self.unit);
        let kind = match tool {
            Tool::Arrow => Kind::Arrow(p, p),
            Tool::Rect => Kind::Rect(p, p),
            Tool::Ellipse => Kind::Ellipse(p, p),
            Tool::Pixelate => Kind::Pixelate(p, p),
            Tool::Pen => Kind::Pen { pts: vec![p], straight: false },
            Tool::Highlight => Kind::Highlight { pts: vec![p], straight: false },
            Tool::Number => {
                let n = st.shapes.iter().filter_map(|s| if let Kind::Number { n, .. } = s.kind { Some(n) } else { None }).max().unwrap_or(0) + 1;
                st.shapes.push(Shape::new(Kind::Number { c: p, n }, c, s, u));
                let i = st.shapes.len() - 1;
                let before = st.before.take().unwrap_or_default();
                st.mode = Mode::Move;
                drop(st);
                self.push_undo(before);
                self.select(Some(i));
                return;
            }
            Tool::Select | Tool::Text => unreachable!(),
        };
        st.active = Some(Shape::new(kind, c, s, u));
        st.mode = Mode::Draw;
        drop(st);
        self.canvas.queue_draw();
    }

    fn drag(&self, x: f32, y: f32, shift: bool) {
        let p = self.to_img(x, y);
        {
            let mut st = self.st.borrow_mut();
            let d = p - st.last;
            match st.mode {
                Mode::Draw => {
                    if let Some(a) = st.active.as_mut() {
                        a.drag_to(p, shift);
                    }
                }
                Mode::Move => {
                    if let Some(i) = st.selected {
                        st.shapes[i].translate(d);
                        st.moved = true;
                    }
                }
                Mode::Handle(h) => {
                    if let Some(i) = st.selected {
                        st.shapes[i].set_handle(h, p, shift);
                        st.moved = true;
                    }
                }
                Mode::Idle => return,
            }
            st.last = p;
        }
        self.canvas.queue_draw();
    }

    fn release(&self, x: f32, y: f32) {
        let (mode, active, before, moved) = {
            let mut st = self.st.borrow_mut();
            // only a click (not the end of a drag) can start a double-click
            let p = self.to_img(x, y);
            if st.last_click.is_some_and(|(_, lp)| (lp - p).len() * self.fit().0 > 4.0) {
                st.last_click = None;
            }
            (std::mem::replace(&mut st.mode, Mode::Idle), st.active.take(), st.before.take(), st.moved)
        };
        match mode {
            Mode::Draw => {
                if let Some(shape) = active.filter(|s| !s.is_degenerate()) {
                    let keep_selected = !matches!(shape.kind, Kind::Pen { .. } | Kind::Highlight { .. });
                    let i = {
                        let mut st = self.st.borrow_mut();
                        st.shapes.push(shape);
                        st.shapes.len() - 1
                    };
                    self.push_undo(before.unwrap_or_default());
                    if keep_selected {
                        self.select(Some(i));
                    }
                }
            }
            Mode::Move | Mode::Handle(_) if moved => self.push_undo(before.unwrap_or_default()),
            _ => {}
        }
        self.canvas.queue_draw();
        self.update_cursor(Some((x, y)));
    }

    // ---- text -------------------------------------------------------------------------
    fn text_px_view(&self) -> f32 {
        let size = self.st.borrow().size;
        shapes::text_font_px(size, self.unit) * self.fit().0
    }

    fn restyle_text(&self) {
        if self.st.borrow().text.is_none() {
            return;
        }
        let c = self.st.borrow().color;
        let halo = if shapes::is_light(&c) { "rgba(20,20,20,0.7)" } else { "rgba(255,255,255,0.8)" };
        let hex = format!("#{:02x}{:02x}{:02x}", (c.red() * 255.0) as u8, (c.green() * 255.0) as u8, (c.blue() * 255.0) as u8);
        let px = self.text_px_view();
        self.text_css.load_from_string(&format!(
            "textview.hs-text, textview.hs-text > text {{ background-color: {halo}; color: {hex}; caret-color: {hex}; \
             font-family: Sans; font-weight: 600; font-size: {px:.1}px; }}\n\
             textview.hs-text {{ border: 1px dashed {hex}; border-radius: 3px; padding: 0 2px; }}"
        ));
    }

    fn start_text(self: &Rc<Self>, p: P, existing: Option<usize>) {
        self.finish_text();
        let (pos, text) = {
            let mut st = self.st.borrow_mut();
            match existing.and_then(|i| st.shapes.get(i).map(|s| (i, s.clone()))) {
                Some((i, s)) => {
                    st.shapes[i].hidden = true;
                    st.color = s.color;
                    st.size = s.size;
                    match s.kind {
                        Kind::Text { pos, text } => (pos, text),
                        _ => unreachable!(),
                    }
                }
                None => (p, String::new()),
            }
        };
        self.sync_toolbar();
        let view = &self.tv;
        view.buffer().set_text(&text);
        let wp = self.to_widget(pos);
        view.set_margin_start((wp.x - 3.0).max(0.0) as i32);
        view.set_margin_top((wp.y - 1.0).max(0.0) as i32);
        self.st.borrow_mut().text = Some(TextEdit { pos, editing: existing });
        self.restyle_text();
        view.set_visible(true);
        view.grab_focus();
        let buf = view.buffer();
        buf.place_cursor(&buf.end_iter());
        self.canvas.queue_draw();
    }

    fn finish_text(&self) {
        let Some(te) = self.st.borrow_mut().text.take() else { return };
        let buf = self.tv.buffer();
        let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim_end().to_string();
        // Move focus away first so the input method detaches cleanly.
        self.canvas.grab_focus();
        self.tv.set_visible(false);
        let snap = {
            let mut st = self.st.borrow_mut();
            if let Some(i) = te.editing {
                st.shapes[i].hidden = false;
            }
            st.shapes.clone()
        };
        let (color, size) = {
            let st = self.st.borrow();
            (st.color, st.size)
        };
        let mut changed = false;
        let mut select = None;
        {
            let mut st = self.st.borrow_mut();
            match te.editing {
                Some(i) if text.trim().is_empty() => {
                    st.shapes.remove(i);
                    changed = true;
                }
                Some(i) => {
                    let s = &mut st.shapes[i];
                    let old = if let Kind::Text { text, .. } = &s.kind { text.clone() } else { String::new() };
                    if old != text || !same_color(&s.color, &color) || s.size != size {
                        s.kind = Kind::Text { pos: te.pos, text: text.clone() };
                        s.color = color;
                        s.size = size;
                        changed = true;
                    }
                    select = Some(i);
                }
                None if !text.trim().is_empty() => {
                    st.shapes.push(Shape::new(Kind::Text { pos: te.pos, text }, color, size, self.unit));
                    select = Some(st.shapes.len() - 1);
                    changed = true;
                }
                None => {}
            }
        }
        if changed {
            self.push_undo(snap);
        }
        self.select(select);
        self.canvas.grab_focus();
        self.canvas.queue_draw();
    }

    // ---- output -------------------------------------------------------------------------
    fn render(&self) -> Option<Img> {
        self.finish_text();
        let pctx = self.canvas.pango_context();
        let ctx = Ctx { base: &self.base, base_tex: &self.base_tex, pango: &pctx };
        let sn = gtk::Snapshot::new();
        {
            let st = self.st.borrow();
            shapes::render(&sn, &ctx, &st.shapes, None, gsk::ScalingFilter::Linear);
        }
        let node = sn.to_node()?;
        let renderer = self.canvas.native()?.renderer()?;
        let tex = renderer.render_texture(node, Some(&rect(0.0, 0.0, self.base.w as f32, self.base.h as f32)));
        Some(Img::from_texture(&tex, self.base.scale))
    }

    fn copy(self: &Rc<Self>) {
        if self.st.borrow().text.is_some() {
            return; // the text box handles Ctrl+C itself
        }
        let Some(img) = self.render() else { return };
        let me = self.clone();
        glib::spawn_future_local(async move {
            let ok = gio::spawn_blocking(move || {
                let mut png = Vec::with_capacity(4 << 20);
                img.write_png_with(&mut png, png::Compression::Fast, png::Filter::Adaptive).is_ok()
                    && util::clipboard_png_bytes(png)
            })
            .await
            .unwrap_or(false);
            me.flash(if ok { "Copied ✓" } else { "Copy failed" });
        });
    }

    fn save(self: &Rc<Self>, close: bool) {
        let path = self.st.borrow().path.clone();
        match path {
            Some(p) => self.write(p, close),
            None => self.save_as(),
        }
    }

    fn save_as(self: &Rc<Self>) {
        self.finish_text();
        let path = self.st.borrow().path.clone();
        let folder = path.as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| util::config().save_dir.clone());
        let name = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Screenshot.png".into());
        let (me, me2) = (self.clone(), self.clone());
        let (folder2, name2) = (folder.clone(), name.clone());
        util::portal_save_file(
            "Save screenshot",
            &name,
            &folder,
            move |p| {
                if let Some(p) = p {
                    me.write(with_ext(p), false);
                }
            },
            move || me2.save_as_gtk(&folder2, &name2),
        );
    }

    /// Used only if the desktop portal isn't running.
    fn save_as_gtk(self: &Rc<Self>, folder: &std::path::Path, name: &str) {
        let dialog = gtk::FileDialog::builder().title("Save screenshot").modal(true).initial_name(name).build();
        dialog.set_initial_folder(Some(&gio::File::for_path(folder)));
        let me = self.clone();
        dialog.save(Some(&self.win), gio::Cancellable::NONE, move |res| {
            if let Some(p) = res.ok().and_then(|f| f.path()) {
                me.write(with_ext(p), false);
            }
        });
    }

    fn write(self: &Rc<Self>, path: PathBuf, close: bool) {
        let Some(img) = self.render() else { return };
        let Some(hs) = self.hs.upgrade() else { return };
        let me = self.clone();
        let hold = hs.app.hold();
        glib::spawn_future_local(async move {
            // never race the original background save of this file
            hs.wait_saved(&path).await;
            let p = path.clone();
            let r = gio::spawn_blocking(move || save_any(&img, &p)).await.unwrap_or_else(|_| Err("save failed".into()));
            match r {
                Ok(()) => {
                    {
                        let mut st = me.st.borrow_mut();
                        st.dirty = false;
                        st.path = Some(path.clone());
                    }
                    me.update_title();
                    me.flash("Saved ✓");
                    if close {
                        me.st.borrow_mut().allow_close = true;
                        me.win.close();
                    }
                }
                Err(e) => {
                    gtk::AlertDialog::builder().message("Couldn't save").detail(e).build().show(Some(&me.win));
                }
            }
            drop(hold);
        });
    }

    fn close_request(self: &Rc<Self>) -> glib::Propagation {
        self.finish_text();
        let (dirty, allow) = {
            let st = self.st.borrow();
            (st.dirty, st.allow_close)
        };
        if !dirty || allow {
            return glib::Propagation::Proceed;
        }
        let dialog = gtk::AlertDialog::builder()
            .message("Save your annotations before closing?")
            .buttons(["Cancel", "Don't Save", "Save"])
            .cancel_button(0)
            .default_button(2)
            .build();
        let me = self.clone();
        dialog.choose(Some(&self.win), gio::Cancellable::NONE, move |r| match r {
            Ok(1) => {
                me.st.borrow_mut().allow_close = true;
                me.win.close();
            }
            Ok(2) => me.save(true),
            _ => {}
        });
        glib::Propagation::Stop
    }
}

impl Editor {
    /// `holyshot __selftest IN OUT`: exercises tools through the same handlers
    /// the mouse uses, then exports.
    pub fn close(&self) {
        self.win.close();
    }

    pub fn selftest(ed: &Rc<Editor>, out: PathBuf) {
        let ed = ed.clone();
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            let (w, h) = (ed.canvas.width() as f32, ed.canvas.height() as f32);
            let at = |fx: f32, fy: f32| (w * fx, h * fy);
            let stroke = |tool: Tool, a: (f32, f32), b: (f32, f32), shift: bool| {
                ed.set_tool(tool);
                ed.press(a.0, a.1, shift);
                for i in 1..=8 {
                    let t = i as f32 / 8.0;
                    let y = a.1 + (b.1 - a.1) * t + if tool == Tool::Pen { (t * 9.0).sin() * 20.0 } else { 0.0 };
                    ed.drag(a.0 + (b.0 - a.0) * t, y, shift);
                }
                ed.release(b.0, b.1);
            };
            stroke(Tool::Arrow, at(0.1, 0.15), at(0.3, 0.35), false);
            ed.set_color(util::hex(COLORS[4].1));
            stroke(Tool::Rect, at(0.35, 0.1), at(0.55, 0.3), false);
            stroke(Tool::Ellipse, at(0.6, 0.1), at(0.8, 0.3), false);
            ed.set_color(util::hex(COLORS[0].1));
            stroke(Tool::Pen, at(0.1, 0.5), at(0.4, 0.5), false);
            stroke(Tool::Highlight, at(0.45, 0.45), at(0.7, 0.45), true);
            stroke(Tool::Pixelate, at(0.75, 0.4), at(0.95, 0.6), false);
            ed.set_tool(Tool::Number);
            for x in [0.2f32, 0.25, 0.3] {
                ed.press(w * x, h * 0.7, false);
                ed.release(w * x, h * 0.7);
            }
            // move the first arrow via the select tool, then recolour it
            ed.set_tool(Tool::Select);
            let mid = at(0.2, 0.25);
            ed.press(mid.0, mid.1, false);
            ed.drag(mid.0 + 40.0, mid.1 + 60.0, false);
            ed.release(mid.0 + 40.0, mid.1 + 60.0);
            ed.set_color(util::hex(COLORS[3].1));
            ed.set_size(3);
            // undo + redo the recolour
            ed.undo();
            ed.redo();
            // text
            ed.set_tool(Tool::Text);
            let t = at(0.4, 0.75);
            ed.press(t.0, t.1, false);
            if ed.st.borrow().text.is_some() {
                ed.tv.buffer().set_text("Hello from HolyShot");
            }
            ed.finish_text();
            let st = ed.st.borrow();
            println!("shapes={} undo={} redo={} selected={:?}", st.shapes.len(), st.undo.len(), st.redo.len(), st.selected);
            let theme = gtk::IconTheme::for_display(&WidgetExt::display(&ed.win));
            let missing: Vec<_> = TOOLS.iter().map(|t| t.2).filter(|n| !theme.has_icon(n)).collect();
            println!("icon theme={:?} missing tool icons={missing:?}", theme.theme_name());
            drop(st);
            match ed.render() {
                Some(img) => println!("export: {:?}", img.save_png(&out)),
                None => println!("export failed"),
            }
            ed.st.borrow_mut().dirty = false;
        });
    }
}

fn with_ext(mut p: PathBuf) -> PathBuf {
    if p.extension().is_none() {
        p.set_extension("png");
    }
    p
}

fn save_any(img: &Img, path: &std::path::Path) -> Result<(), String> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "jpg" || ext == "jpeg" {
        let mut rgb = Vec::with_capacity(img.w * img.h * 3);
        for y in 0..img.h {
            let row = &img.bytes[y * img.stride..y * img.stride + img.w * 4];
            for p in row.chunks_exact(4) {
                rgb.extend_from_slice(&[p[2], p[1], p[0]]);
            }
        }
        let pb = gtk::gdk_pixbuf::Pixbuf::from_bytes(
            &glib::Bytes::from_owned(rgb),
            gtk::gdk_pixbuf::Colorspace::Rgb,
            false,
            8,
            img.w as i32,
            img.h as i32,
            (img.w * 3) as i32,
        );
        pb.savev(path, "jpeg", &[("quality", "92")]).map_err(|e| e.to_string())
    } else {
        img.save_png(path)
    }
}
