//! The preview that slides into the bottom-right corner after a capture.
//! Click: annotate (a recording: play it). Drag: drop the file into another
//! app. Hover: keep it. Right-click: more options.

use gtk::{gdk, gio, glib, graphene, gsk, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use crate::draw::DrawArea;
use crate::img::Img;
use crate::util::{self, rect, rgba};
use crate::Hs;

const MAX_W: f32 = 300.0;
const MAX_H: f32 = 190.0;
const PAD: f32 = 18.0;
const MARGIN: i32 = 6;
const RADIUS: f32 = 10.0;

struct State {
    offset: f32,
    anim: Option<(f32, f32, i64, f32)>, // from, to, start µs, duration µs
    dismissing: bool,
    hover: bool,
    timer: Option<glib::SourceId>,
    press: Option<(f32, f32)>,
    dragging: bool,
    closed: bool,
    menu: Option<gtk::PopoverMenu>,
}

pub struct Thumb {
    hs: Weak<Hs>,
    win: gtk::Window,
    area: DrawArea,
    /// The preview never holds the full image: the save thread owns it (and
    /// encodes it in place), then the file serves the editor and drag-and-
    /// drop. Saves ~20 MB for the preview's lifetime, the longest phase.
    size: (usize, usize),
    scale: f64,
    path: PathBuf,
    /// A screen recording of this many seconds, rather than a screenshot.
    video: Option<u64>,
    /// The preview at exactly its on-screen pixel size (see `show`).
    small: gdk::Texture,
    /// Shadow + image + border, rendered once (lazily, at device scale) so
    /// each animation frame is a single blit instead of a CPU blur.
    card: RefCell<Option<gdk::Texture>>,
    tw: f32,
    th: f32,
    st: RefCell<State>,
}

impl Thumb {
    /// Device-pixel size of the preview for an image shown at `dpr`.
    pub fn preview_size(img: &Img, dpr: f64) -> (usize, usize) {
        let dpr = dpr.max(1.0) as f32;
        let (lw, lh) = (img.w as f32 / img.scale as f32, img.h as f32 / img.scale as f32);
        let k = (MAX_W / lw).min(MAX_H / lh).min(1.0);
        (((lw * k).max(48.0) * dpr).round() as usize, ((lh * k).max(32.0) * dpr).round() as usize)
    }

    /// `preview` must be `preview_size` for the monitor (the capture thread
    /// computes it in parallel with GTK start-up for whole-screen shots).
    pub fn show(
        hs: &Rc<Hs>,
        preview: Img,
        size: (usize, usize),
        path: PathBuf,
        monitor: Option<gdk::Monitor>,
        video: Option<u64>,
    ) -> Rc<Thumb> {
        // Pre-scaled to the exact device-pixel size: the CPU renderer would
        // otherwise resample the full capture every frame (~60 ms CPU each).
        let dpr = monitor.as_ref().map(|m| m.scale()).unwrap_or(preview.scale).max(1.0) as f32;
        let scale = preview.scale;
        let small = preview.texture();
        let (tw, th) = (small.width() as f32 / dpr, small.height() as f32 / dpr);

        let win = gtk::Window::new();
        win.set_application(Some(&hs.app));
        win.init_layer_shell();
        win.set_layer(Layer::Overlay);
        win.set_namespace(Some(util::LAYER_NAMESPACE));
        if let Some(m) = &monitor {
            win.set_monitor(Some(m));
        }
        win.set_anchor(Edge::Bottom, true);
        win.set_anchor(Edge::Right, true);
        win.set_margin(Edge::Bottom, MARGIN);
        win.set_margin(Edge::Right, MARGIN); // updated per frame by `animate`
        win.set_keyboard_mode(KeyboardMode::None);
        win.set_decorated(false);
        win.add_css_class("hs-layer");
        let (ww, wh) = ((tw + 2.0 * PAD).ceil() as i32, (th + 2.0 * PAD).ceil() as i32);
        win.set_default_size(ww, wh);

        let area = DrawArea::new();
        area.set_size_request(ww, wh);
        area.set_cursor_from_name(Some("pointer"));
        win.set_child(Some(&area));

        util::trace("thumbnail: window built");
        let t = Rc::new(Thumb {
            hs: Rc::downgrade(hs),
            win,
            area,
            size,
            scale,
            path,
            video,
            small,
            card: RefCell::new(None),
            tw,
            th,
            st: RefCell::new(State {
                offset: ww as f32,
                anim: None,
                dismissing: false,
                hover: false,
                timer: None,
                press: None,
                dragging: false,
                closed: false,
                menu: None,
            }),
        });
        t.connect();
        t.win.set_margin(Edge::Right, MARGIN - ww);
        util::trace_first_paint(&t.win, "thumbnail: first paint");
        t.win.present();
        util::trace("thumbnail presented");
        t.animate(0.0, 260.0);
        t.arm_timer(util::config().thumbnail_seconds);
        t
    }

    fn connect(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.area.set_draw(move |_, sn, _, _| {
            if let Some(t) = w.upgrade() {
                t.draw(sn);
            }
        });

        let motion = gtk::EventControllerMotion::new();
        let w = Rc::downgrade(self);
        motion.connect_enter(move |_, _, _| {
            if let Some(t) = w.upgrade() {
                t.disarm_timer();
                let back = {
                    let mut st = t.st.borrow_mut();
                    st.hover = true;
                    std::mem::replace(&mut st.dismissing, false)
                };
                if back {
                    t.animate(0.0, 160.0); // caught it on the way out
                }
                t.area.queue_draw();
            }
        });
        let w = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(t) = w.upgrade() {
                let menu_open = {
                    let mut st = t.st.borrow_mut();
                    st.hover = false;
                    st.menu.as_ref().is_some_and(|m| m.is_visible()) || st.dragging
                };
                if !menu_open {
                    t.arm_timer(3.0);
                }
                t.area.queue_draw();
            }
        });
        self.area.add_controller(motion);

        let click = gtk::GestureClick::new();
        click.set_button(1);
        let w = Rc::downgrade(self);
        click.connect_pressed(move |_, _, x, y| {
            if let Some(t) = w.upgrade() {
                t.st.borrow_mut().press = Some((x as f32, y as f32));
            }
        });
        let w = Rc::downgrade(self);
        click.connect_released(move |_, _, x, y| {
            if let Some(t) = w.upgrade() {
                let press = t.st.borrow_mut().press.take();
                if press.is_none() || t.st.borrow().dragging {
                    return;
                }
                let p = graphene::Point::new(x as f32, y as f32);
                if t.close_rect().contains_point(&p) {
                    t.dismiss();
                } else if t.thumb_rect().contains_point(&p) {
                    t.activate();
                }
            }
        });
        self.area.add_controller(click);

        // Drag the screenshot out as a file (and as image data for apps that
        // prefer that).
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::COPY);
        let w = Rc::downgrade(self);
        drag.connect_prepare(move |_, _, _| {
            let t = w.upgrade()?;
            let file = gio::File::for_path(&t.path);
            let files = gdk::FileList::from_array(&[file]);
            let files = gdk::ContentProvider::for_value(&files.to_value());
            if t.video.is_some() {
                return Some(files);
            }
            // also offer the image itself, for apps that take pasted images
            Some(match std::fs::read(&t.path) {
                Ok(png) => gdk::ContentProvider::new_union(&[
                    files,
                    gdk::ContentProvider::for_bytes("image/png", &glib::Bytes::from_owned(png)),
                ]),
                Err(_) => files, // still being written (first ~30 ms)
            })
        });
        let w = Rc::downgrade(self);
        drag.connect_drag_begin(move |src, _| {
            if let Some(t) = w.upgrade() {
                t.disarm_timer();
                {
                    let mut st = t.st.borrow_mut();
                    st.dragging = true;
                    st.press = None;
                }
                let pic = gtk::Picture::for_paintable(&t.small);
                pic.set_size_request((t.tw * 0.6) as i32, (t.th * 0.6) as i32);
                pic.set_can_shrink(true);
                let icon = gtk::DragIcon::for_drag(&src.drag().unwrap());
                icon.set_child(Some(&pic));
            }
        });
        let w = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(t) = w.upgrade() {
                t.st.borrow_mut().dragging = false;
                t.dismiss();
            }
        });
        self.area.add_controller(drag);

        let right = gtk::GestureClick::new();
        right.set_button(3);
        let w = Rc::downgrade(self);
        right.connect_pressed(move |_, _, x, y| {
            if let Some(t) = w.upgrade() {
                t.menu(x, y);
            }
        });
        self.area.add_controller(right);

        let actions = gio::SimpleActionGroup::new();
        for name in ["annotate", "open", "copy", "folder", "delete", "close"] {
            let a = gio::SimpleAction::new(name, None);
            let w = Rc::downgrade(self);
            a.connect_activate(move |a, _| {
                if let Some(t) = w.upgrade() {
                    t.action(a.name().as_str());
                }
            });
            actions.add_action(&a);
        }
        self.win.insert_action_group("thumb", Some(&actions));
    }

    fn menu(self: &Rc<Self>, x: f64, y: f64) {
        self.disarm_timer();
        let existing = self.st.borrow().menu.clone();
        let pop = existing.unwrap_or_else(|| {
            let m = gio::Menu::new();
            if self.video.is_some() {
                m.append(Some("Open"), Some("thumb.open"));
            } else {
                m.append(Some("Annotate…"), Some("thumb.annotate"));
                m.append(Some("Copy"), Some("thumb.copy"));
            }
            m.append(Some("Show in Folder"), Some("thumb.folder"));
            let m2 = gio::Menu::new();
            m2.append(Some("Move to Trash"), Some("thumb.delete"));
            m2.append(Some("Close"), Some("thumb.close"));
            m.append_section(None, &m2);
            let pop = gtk::PopoverMenu::from_model(Some(&m));
            pop.set_parent(&self.area);
            pop.set_has_arrow(false);
            let w = Rc::downgrade(self);
            pop.connect_closed(move |_| {
                if let Some(t) = w.upgrade() {
                    if !t.st.borrow().hover && !t.st.borrow().dismissing {
                        t.arm_timer(3.0);
                    }
                }
            });
            self.st.borrow_mut().menu = Some(pop.clone());
            pop
        });
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.popup();
    }

    fn action(self: &Rc<Self>, name: &str) {
        let Some(hs) = self.hs.upgrade() else { return };
        let path = self.path.clone();
        match name {
            "annotate" => self.open_editor(),
            "open" => self.activate(),
            "close" => self.dismiss(),
            _ => {
                let what = name.to_string();
                let me = self.clone();
                glib::spawn_future_local(async move {
                    hs.wait_saved(&path).await;
                    match what.as_str() {
                        "copy" => {
                            util::clipboard_png_file(&path);
                        }
                        "folder" => util::show_in_folder(&path),
                        "delete" => {
                            let _ = gio::File::for_path(&path).trash(gio::Cancellable::NONE);
                        }
                        _ => {}
                    }
                    me.dismiss();
                });
            }
        }
    }

    pub fn test_menu(self: &Rc<Self>) {
        self.menu(60.0, 60.0);
    }

    pub fn img_size(&self) -> (usize, usize) {
        self.size
    }

    /// Click: annotate a screenshot, play a recording.
    fn activate(self: &Rc<Self>) {
        if self.video.is_none() {
            return self.open_editor();
        }
        let uri = gio::File::for_path(&self.path).uri();
        if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, Some(&util::launch_context())) {
            util::notify("Couldn't open the recording", &e.to_string());
        }
        self.dismiss();
    }

    pub fn open_editor(self: &Rc<Self>) {
        let Some(hs) = self.hs.upgrade() else { return };
        let (path, scale) = (self.path.clone(), self.scale);
        // Keep the app alive across the hand-over (the preview closes first).
        let hold = hs.app.hold();
        self.close_now();
        glib::spawn_future_local(async move {
            hs.wait_saved(&path).await;
            hs.edit_file_at_scale(&path, scale);
            drop(hold);
        });
    }

    // ---- timers & animation ---------------------------------------------------
    fn arm_timer(self: &Rc<Self>, secs: f64) {
        self.disarm_timer();
        let w = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_secs_f64(secs), move || {
            if let Some(t) = w.upgrade() {
                t.st.borrow_mut().timer = None;
                t.dismiss();
            }
        });
        self.st.borrow_mut().timer = Some(id);
    }

    fn disarm_timer(&self) {
        let id = self.st.borrow_mut().timer.take();
        if let Some(id) = id {
            util::remove_source(id);
        }
    }

    pub fn dismiss(self: &Rc<Self>) {
        util::trace("thumbnail: dismiss");
        {
            let mut st = self.st.borrow_mut();
            if st.dismissing {
                return;
            }
            st.dismissing = true;
        }
        self.disarm_timer();
        self.animate(self.area.width() as f32, 200.0);
        // Frame callbacks stop when the screen is off or locked, which would
        // leave the slide-out (and the process) hanging; close regardless.
        let w = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(450), move || {
            if let Some(t) = w.upgrade() {
                if t.st.borrow().dismissing {
                    t.close_now();
                }
            }
        });
    }

    pub fn close_now(&self) {
        if std::mem::replace(&mut self.st.borrow_mut().closed, true) {
            return;
        }
        util::trace("thumbnail: close_now");
        self.disarm_timer();
        {
            let mut st = self.st.borrow_mut();
            st.dismissing = true;
            st.anim = None;
            if let Some(m) = st.menu.take() {
                m.unparent();
            }
        }
        if let Some(hs) = self.hs.upgrade() {
            hs.thumb_closed(self);
        }
        self.win.destroy();
    }

    fn animate(self: &Rc<Self>, to: f32, ms: f32) {
        let start = self.area.frame_clock().map(|c| c.frame_time()).unwrap_or_else(glib::monotonic_time);
        let from = self.st.borrow().offset;
        let running = self.st.borrow().anim.is_some();
        self.st.borrow_mut().anim = Some((from, to, start, ms * 1000.0));
        if running {
            return; // the existing tick callback picks up the new target
        }
        let w = Rc::downgrade(self);
        self.area.add_tick_callback(move |_, clock| {
            let Some(t) = w.upgrade() else { return glib::ControlFlow::Break };
            let Some((from, to, start, dur)) = t.st.borrow().anim else { return glib::ControlFlow::Break };
            let k = ((clock.frame_time() - start) as f32 / dur).clamp(0.0, 1.0);
            let out = to < from; // sliding in
            let e = if out { 1.0 - (1.0 - k).powi(3) } else { k * k * k };
            let off = from + (to - from) * e;
            t.st.borrow_mut().offset = off;
            // Slide by moving the surface (layer-shell margin), not by
            // redrawing it: the compositor repositions, our pixels don't change.
            t.win.set_margin(Edge::Right, MARGIN - off.round() as i32);
            util::trace_once("thumbnail: first anim tick");
            if k < 1.0 {
                return glib::ControlFlow::Continue;
            }
            t.st.borrow_mut().anim = None;
            if t.st.borrow().dismissing {
                t.close_now();
            }
            glib::ControlFlow::Break
        });
    }

    // ---- drawing -----------------------------------------------------------------
    fn thumb_rect(&self) -> graphene::Rect {
        // keep it on whole device pixels so the blit stays 1:1
        let dpr = self.area.native().and_then(|n| n.surface()).map(|s| s.scale() as f32).unwrap_or(1.0);
        let x = (PAD * dpr).round() / dpr;
        let y = (PAD * dpr).round() / dpr;
        rect(x, y, self.tw, self.th)
    }

    fn close_rect(&self) -> graphene::Rect {
        let r = self.thumb_rect();
        rect(r.x() - 9.0, r.y() - 9.0, 22.0, 22.0)
    }

    fn dpr(&self) -> f32 {
        self.area.native().and_then(|n| n.surface()).map(|s| s.scale() as f32).unwrap_or(1.0)
    }

    fn card(&self) -> Option<gdk::Texture> {
        if let Some(t) = self.card.borrow().as_ref() {
            return Some(t.clone());
        }
        let dpr = self.dpr();
        let (w, h) = (self.area.width() as f32, self.area.height() as f32);
        let r = rect(PAD, PAD, self.tw, self.th);
        let rr = util::rounded(r, RADIUS);
        let sn = gtk::Snapshot::new();
        sn.scale(dpr, dpr);
        sn.append_outset_shadow(&rr, &rgba(0.0, 0.0, 0.0, 0.45), 0.0, 4.0, 0.0, 14.0);
        sn.push_rounded_clip(&rr);
        sn.append_texture(&self.small, &r);
        sn.pop();
        sn.append_border(&util::rounded(r.inset_r(1.0, 1.0), RADIUS - 1.0), &[2.0; 4], &[rgba(1.0, 1.0, 1.0, 0.85); 4]);
        sn.append_border(&rr, &[1.0; 4], &[rgba(0.0, 0.0, 0.0, 0.25); 4]);
        if let Some(secs) = self.video {
            // play badge with the length, bottom-left
            let l = util::layout(&self.area, &format!("▶  {}:{:02}", secs / 60, secs % 60), 9.5, true);
            let (_, lh) = l.pixel_size();
            util::pill(&sn, &l, r.x() + 8.0, r.y() + r.height() - lh as f32 - 16.0, 8.0, 4.0, 6.0);
        }
        let node = sn.to_node()?;
        let renderer = gsk::CairoRenderer::new();
        renderer.realize_for_display(&WidgetExt::display(&self.area)).ok()?;
        let tex = renderer.render_texture(node, Some(&rect(0.0, 0.0, (w * dpr).round(), (h * dpr).round())));
        renderer.unrealize();
        *self.card.borrow_mut() = Some(tex.clone());
        Some(tex)
    }

    fn draw(&self, sn: &gtk::Snapshot) {
        let r = self.thumb_rect();
        if let Some(card) = self.card() {
            let dpr = self.dpr();
            sn.append_texture(&card, &rect(r.x() - PAD, 0.0, card.width() as f32 / dpr, card.height() as f32 / dpr));
        }
        let st = self.st.borrow();
        if st.hover {
            sn.push_rounded_clip(&util::rounded(r, RADIUS));
            sn.append_color(&rgba(1.0, 1.0, 1.0, 0.1), &r);
            sn.pop();
        }

        if st.hover && !st.dismissing {
            let c = self.close_rect();
            let pb = gsk::PathBuilder::new();
            let (cx, cy) = (c.x() + 11.0, c.y() + 11.0);
            pb.add_circle(&graphene::Point::new(cx, cy), 11.0);
            let circle = pb.to_path();
            sn.append_fill(&circle, gsk::FillRule::Winding, &rgba(0.12, 0.12, 0.12, 0.94));
            let edge = gsk::Stroke::new(1.5);
            sn.append_stroke(&circle, &edge, &rgba(1.0, 1.0, 1.0, 0.9));
            let x = gsk::PathBuilder::new();
            x.move_to(cx - 4.0, cy - 4.0);
            x.line_to(cx + 4.0, cy + 4.0);
            x.move_to(cx + 4.0, cy - 4.0);
            x.line_to(cx - 4.0, cy + 4.0);
            let s = gsk::Stroke::new(2.0);
            s.set_line_cap(gsk::LineCap::Round);
            sn.append_stroke(&x.to_path(), &s, &gdk::RGBA::WHITE);
        }
    }
}
