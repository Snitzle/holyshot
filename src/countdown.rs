//! The self-timer: a big translucent number counting down in the middle of
//! what will be captured. It never takes the keyboard and lets clicks through,
//! so you can open the menu or hover state you want to capture meanwhile.
//! Pressing the shortcut again cancels it.

use gtk::{gdk, gio, glib, graphene, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::draw::DrawArea;
use crate::util::{self, rgba};
use crate::Hs;

const SIZE: i32 = 96;

pub struct Countdown {
    win: gtk::Window,
    left: Cell<u32>,
    tick: RefCell<Option<glib::SourceId>>,
    then: RefCell<Option<Box<dyn FnOnce()>>>,
    _hold: gio::ApplicationHoldGuard,
}

impl Countdown {
    /// Counts `secs` down centred on `at` (global logical coordinates, on
    /// `mon`), then hides itself and calls `then` once it's off the screen.
    pub fn start(hs: &Rc<Hs>, secs: u32, mon: &gdk::Monitor, at: (i32, i32), then: impl FnOnce() + 'static) -> Rc<Countdown> {
        let g = mon.geometry();
        let x = (at.0 - g.x() - SIZE / 2).clamp(0, (g.width() - SIZE).max(0));
        let y = (at.1 - g.y() - SIZE / 2).clamp(0, (g.height() - SIZE).max(0));

        let win = gtk::Window::new();
        win.set_application(Some(&hs.app));
        win.init_layer_shell();
        win.set_layer(Layer::Overlay);
        win.set_namespace(Some(util::LAYER_NAMESPACE));
        win.set_monitor(Some(mon));
        win.set_anchor(Edge::Top, true);
        win.set_anchor(Edge::Left, true);
        win.set_margin(Edge::Left, x);
        win.set_margin(Edge::Top, y);
        win.set_keyboard_mode(KeyboardMode::None);
        win.set_decorated(false);
        win.add_css_class("hs-layer");
        win.set_default_size(SIZE, SIZE);
        let area = DrawArea::new();
        area.set_size_request(SIZE, SIZE);
        win.set_child(Some(&area));
        // clicks go to whatever is underneath (set again once mapped, in
        // case GTK recomputed the region while sizing the window)
        let through = |w: &gtk::Window| {
            if let Some(s) = w.surface() {
                s.set_input_region(Some(&gtk::cairo::Region::create()));
            }
        };
        win.connect_realize(through);
        win.connect_map(through);

        let cd = Rc::new(Countdown {
            win,
            left: Cell::new(secs),
            tick: RefCell::default(),
            then: RefCell::new(Some(Box::new(then))),
            _hold: hs.app.hold(),
        });
        let w = Rc::downgrade(&cd);
        area.set_draw(move |widget, sn, _, _| {
            if let Some(cd) = w.upgrade() {
                cd.draw(widget, sn);
            }
        });
        let w = Rc::downgrade(&cd);
        let a = area.clone();
        // (not timeout_add_seconds: that snaps to whole seconds, ±1 s)
        let id = glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(cd) = w.upgrade() else { return glib::ControlFlow::Break };
            let left = cd.left.get().saturating_sub(1);
            cd.left.set(left);
            if left > 0 {
                a.queue_draw();
                return glib::ControlFlow::Continue;
            }
            cd.tick.borrow_mut().take(); // this source ends by returning Break
            cd.win.set_visible(false);
            let then = cd.then.borrow_mut().take();
            let win = cd.win.clone();
            // give KWin a frame or two to take it off the screen
            glib::timeout_add_local_once(Duration::from_millis(80), move || {
                win.destroy();
                if let Some(f) = then {
                    f();
                }
            });
            glib::ControlFlow::Break
        });
        *cd.tick.borrow_mut() = Some(id);
        cd.win.present();
        cd
    }

    /// True if it was still counting.
    pub fn cancel(&self) -> bool {
        let running = self.then.borrow_mut().take().is_some();
        if let Some(id) = self.tick.borrow_mut().take() {
            id.remove();
        }
        self.win.destroy();
        running
    }

    fn draw(&self, widget: &DrawArea, sn: &gtk::Snapshot) {
        let s = SIZE as f32;
        let r = graphene::Rect::new(0.0, 0.0, s, s);
        let rr = gtk::gsk::RoundedRect::from_rect(r, s / 2.0);
        sn.push_rounded_clip(&rr);
        sn.append_color(&rgba(0.08, 0.08, 0.08, 0.72), &r);
        sn.pop();
        sn.append_border(&rr, &[1.5; 4], &[rgba(1.0, 1.0, 1.0, 0.35); 4]);
        let l = util::layout(widget, &self.left.get().to_string(), 34.0, true);
        let (lw, lh) = l.pixel_size();
        sn.save();
        sn.translate(&graphene::Point::new((s - lw as f32) / 2.0, (s - lh as f32) / 2.0));
        sn.append_layout(&l, &gdk::RGBA::WHITE);
        sn.restore();
    }
}
