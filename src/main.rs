//! HolyShot: Mac-style screenshots for KDE Plasma (Wayland).
//!
//! There is no resident daemon. Each shortcut press starts this binary;
//! GApplication makes a second press while we're still up (preview showing,
//! editor open) forward to the running instance instead. When the last
//! window closes and pending saves finish, the process exits, so it costs
//! nothing between screenshots.

mod capture;
mod draw;
mod editor;
mod img;
mod overlay;
mod countdown;
mod recorder;
mod shapes;
mod thumbnail;
mod util;

use gtk::{gdk, gio, glib, prelude::*};
use std::cell::{OnceCell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use capture::{Capture, Target};
use img::Img;
use overlay::{Picked, Purpose};

pub const APP_ID: &str = "dev.holyshot";

const USAGE: &str = "\
holyshot [COMMAND]
  area         pick an area of the screen (default; Space switches to windows)
  window       pick a window
  screen       capture the whole screen the mouse is on
  record       record an area, window or screen (run again to stop)
  edit FILE    open an image in the annotation editor";

type Early = std::thread::JoinHandle<Option<Result<Capture, String>>>;

thread_local! {
    /// Capture started before GTK even initialises (see `main`).
    static EARLY: RefCell<Option<(String, Early)>> = const { RefCell::new(None) };
    static HS: OnceCell<Rc<Hs>> = const { OnceCell::new() };
}

fn main() -> glib::ExitCode {
    util::trace_init();
    util::trace("main");
    util::tune_gtk_env();
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("area");
    if matches!(cmd, "-h" | "--help" | "help") {
        println!("{USAGE}");
        return glib::ExitCode::SUCCESS;
    }

    // Undocumented: save the active screen to a PNG with no UI (for testing).
    if cmd == "__dump" {
        let out = args.get(2).cloned().unwrap_or_else(|| "dump.png".into());
        let r = capture::session().and_then(|c| capture::grab(&c, &Target::Active)).and_then(|c| c.img.save_png(std::path::Path::new(&out)));
        if let Err(e) = r {
            eprintln!("holyshot: {e}");
            return glib::ExitCode::FAILURE;
        }
        return glib::ExitCode::SUCCESS;
    }

    // Undocumented: list windows, and save the topmost one (for testing).
    if cmd == "__windows" {
        let conn = capture::session().unwrap();
        let t0 = std::time::Instant::now();
        let r = capture::windows(&conn);
        println!("{:.1} ms: {r:?}", t0.elapsed().as_secs_f64() * 1000.0);
        if let (Ok(ws), Some(out)) = (r, args.get(2)) {
            let c = capture::grab(&conn, &Target::Window(ws[0].id.clone())).unwrap();
            println!("window {}x{} @{}", c.img.w, c.img.h, c.img.scale);
            c.img.save_png(std::path::Path::new(out)).unwrap();
        }
        return glib::ExitCode::SUCCESS;
    }

    if cmd == "__bench-scale" {
        gtk::init().ok();
        let t = gdk::Texture::from_filename(&args[2]).unwrap();
        let img = Img::from_texture(&t, 1.0);
        for (w, h) in [(2750usize, 1151usize), (1720, 720), (345, 144)] {
            let i0 = util::instructions();
            let t0 = std::time::Instant::now();
            let out = img.downscale(w, h);
            println!(
                "{}x{} -> {}x{}: {:.1} M instr, {:.1} ms",
                img.w, img.h, out.w, out.h,
                (util::instructions() - i0) as f64 / 1e6,
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        let i0 = util::instructions();
        let _ = img.texture();
        println!("texture(): {:.1} M", (util::instructions() - i0) as f64 / 1e6);
        return glib::ExitCode::SUCCESS;
    }
    if cmd == "__bench-png" {
        gtk::init().ok();
        for f in &args[2..] {
            match gdk::Texture::from_filename(f) {
                Ok(t) => img::bench_png(&Img::from_texture(&t, 1.0), f),
                Err(e) => eprintln!("{f}: {e}"),
            }
        }
        return glib::ExitCode::SUCCESS;
    }

    // Grab the screen on a worker thread right away, in parallel with GTK
    // start-up. This both hides start-up latency and freezes the screen as
    // close as possible to the moment the key was pressed. If another
    // instance is already running we'll just forward to it, so skip.
    let picker_cmd = matches!(cmd, "area" | "window" | "record");
    if cmd == "record" {
        // Find a working video encoder (~0.6 s the first time after boot)
        // while the user is still picking what to record.
        std::thread::spawn(recorder::encoder);
    }
    if picker_cmd {
        // The picker's first frame draws text; fontconfig's one-time set-up
        // (~19 ms, process-global and thread-safe) would otherwise run on the
        // main thread right then. Do it now, in parallel with GTK start-up.
        std::thread::spawn(|| {
            util::warm_fontconfig();
            util::trace("fontconfig warm");
        });
    }
    if picker_cmd || cmd == "screen" {
        let target = if picker_cmd { Target::Workspace } else { Target::Active };
        let handle = std::thread::spawn(move || {
            let conn = capture::session().ok()?;
            if capture::other_instance_running(&conn) {
                return None;
            }
            let mut r = capture::grab(&conn, &target);
            if let (Ok(c), Target::Workspace) = (&mut r, &target) {
                c.windows = capture::windows(&conn).unwrap_or_default();
            }
            if let (Ok(c), Target::Active) = (&mut r, &target) {
                // the preview's downscale, while GTK is still starting
                let (w, h) = thumbnail::Thumb::preview_size(&c.img, c.img.scale);
                c.preview = Some(c.img.downscale(w, h));
                util::trace("capture: preview ready");
            }
            Some(r)
        });
        EARLY.with(|e| *e.borrow_mut() = Some((cmd.to_string(), handle)));
    }

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    // We ship no GResources; without this GTK probes /dev/holyshot/icons/* for
    // every icon-theme directory at start-up (~650 extra lookups).
    app.set_resource_base_path(None);
    app.connect_startup(|_| {
        util::trace("gtk startup");
        load_css();
    });
    app.connect_command_line(|app, cl| {
        let hs = HS.with(|h| h.get_or_init(|| Hs::new(app)).clone());
        let argv: Vec<String> =
            cl.arguments().into_iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
        hs.handle(&argv, cl.cwd());
        glib::ExitCode::SUCCESS
    });
    util::trace("app built, running");
    let code = app.run_with_args(&args);
    util::trace_counts();
    util::trace("exit");
    code
}

fn load_css() {
    let css = gtk::CssProvider::new();
    css.load_from_string(include_str!("style.css"));
    if let Some(d) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&d, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

pub struct Hs {
    pub app: gtk::Application,
    picker: RefCell<Option<Rc<overlay::Picker>>>,
    thumb: RefCell<Option<Rc<thumbnail::Thumb>>>,
    editors: RefCell<Vec<Rc<editor::Editor>>>,
    recorder: RefCell<Option<Rc<recorder::Recorder>>>,
    countdown: RefCell<Option<Rc<countdown::Countdown>>>,
    /// Files still being written in the background.
    pub saving: RefCell<HashSet<PathBuf>>,
}

impl Hs {
    fn new(app: &gtk::Application) -> Rc<Hs> {
        Rc::new(Hs {
            app: app.clone(),
            picker: RefCell::default(),
            thumb: RefCell::default(),
            editors: RefCell::default(),
            recorder: RefCell::default(),
            countdown: RefCell::default(),
            saving: RefCell::default(),
        })
    }

    fn handle(self: &Rc<Self>, argv: &[String], cwd: Option<PathBuf>) {
        match argv.first().map(String::as_str).unwrap_or("area") {
            "area" => self.pick("area", Purpose::Shot, false),
            "window" => self.pick("window", Purpose::Shot, true),
            "record" => {
                // Pressing it again while recording stops.
                let rec = self.recorder.borrow().clone();
                match rec {
                    Some(r) => r.stop(),
                    None if self.cancel_countdown() => {}
                    None => self.pick("record", Purpose::Record, false),
                }
            }
            "screen" => self.screen(),
            "edit" => {
                for f in &argv[1..] {
                    let p = PathBuf::from(f);
                    let p = if p.is_absolute() { p } else { cwd.clone().unwrap_or_default().join(p) };
                    self.edit_file(&p);
                }
            }
            "__selftest-area" => {
                // Undocumented: `[record] [OVERLAY_PNG]`. With `record`, click
                // Record instead of Capture, and stop after 3 s.
                let record = argv.get(1).is_some_and(|a| a == "record");
                let shot = argv.iter().skip(1).find(|a| a.ends_with(".png")).map(PathBuf::from);
                self.capture("area-test", Target::Workspace, move |hs, c| {
                    let picker = overlay::Picker::start(hs, c.img, c.windows, Purpose::Shot, false);
                    overlay::selftest(&picker, record, shot);
                    *hs.picker.borrow_mut() = Some(picker);
                    if record {
                        let hs2 = hs.clone();
                        glib::timeout_add_local_once(Duration::from_millis(3500), move || {
                            let rec = hs2.recorder.borrow().clone();
                            match rec {
                                Some(r) => r.stop(),
                                None => println!("not recording!"),
                            }
                        });
                        return;
                    }
                    // then: open the editor from the preview, and close it again
                    let hs2 = hs.clone();
                    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                        let t = hs2.thumb.borrow().clone();
                        match t {
                            Some(t) => {
                                println!("thumbnail up: {}x{} px", t.img_size().0, t.img_size().1);
                                t.open_editor();
                            }
                            None => println!("no thumbnail!"),
                        }
                        let hs3 = hs2.clone();
                        glib::timeout_add_local_once(Duration::from_millis(1000), move || {
                            println!("editors open: {}", hs3.editors.borrow().len());
                            let eds = hs3.editors.borrow().clone();
                            for e in eds {
                                e.close();
                            }
                        });
                    });
                });
            }
            "__selftest-timer" if argv.len() > 1 => {
                // Undocumented: see overlay::selftest_timer.
                let out = PathBuf::from(&argv[1]);
                self.capture("timer-test", Target::Workspace, move |hs, c| {
                    let picker = overlay::Picker::start(hs, c.img, c.windows, Purpose::Shot, false);
                    overlay::selftest_timer(&picker, out);
                    *hs.picker.borrow_mut() = Some(picker);
                });
            }
            "__selftest-window" if argv.len() > 1 => {
                // Undocumented: window mode with the topmost window hovered,
                // screenshot of that to argv[1], then a click on it.
                let out = PathBuf::from(&argv[1]);
                self.capture("window-test", Target::Workspace, move |hs, c| {
                    println!("windows: {:?}", c.windows);
                    let picker = overlay::Picker::start(hs, c.img, c.windows, Purpose::Shot, true);
                    *hs.picker.borrow_mut() = Some(picker.clone());
                    let hs2 = hs.clone();
                    glib::timeout_add_local_once(Duration::from_millis(600), move || {
                        println!("hovered: {}", overlay::hover_first_window(&picker, &out));
                        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                            let t = hs2.thumb.borrow().clone();
                            match t {
                                Some(t) => {
                                    println!("window shot: {}x{}", t.img_size().0, t.img_size().1);
                                    t.close_now();
                                }
                                None => println!("no thumbnail!"),
                            }
                        });
                    });
                });
            }
            "__selftest-record" => {
                // Undocumented: record 640x360 at the top-left (or the topmost
                // window with `window`) for 3 s, then stop as the shortcut would.
                let window = argv.get(1).is_some_and(|a| a == "window");
                let screen = argv.get(1).is_some_and(|a| a == "screen");
                let secs: u64 = argv.get(2).and_then(|a| a.parse().ok()).unwrap_or(4);
                if let Some(out) = argv.get(3).map(PathBuf::from) {
                    glib::timeout_add_local_once(Duration::from_secs(2), move || {
                        std::thread::spawn(move || {
                            let r = capture::session().and_then(|c| capture::grab(&c, &Target::Active)).and_then(|c| c.img.save_png(&out));
                            println!("screenshot: {r:?}");
                        });
                    });
                }
                self.capture("record-test", Target::Workspace, move |hs, c| {
                    let mon = util::monitors().into_iter().next().unwrap();
                    let g = mon.geometry();
                    let s = c.img.scale;
                    let p = match c.windows.first() {
                        Some(w) if window => Picked::Window { win: w.clone(), img: c.img.crop(0, 0, 64, 64), mon },
                        _ if screen => {
                            let img = c.img.crop(0, 0, (g.width() as f64 * s) as usize, (g.height() as f64 * s) as usize);
                            Picked::Area { img, mon, rect: (g.x(), g.y(), g.width(), g.height()) }
                        }
                        _ => {
                            let img = c.img.crop(0, 0, (640.0 * s) as usize, (360.0 * s) as usize);
                            Picked::Area { img, mon, rect: (g.x(), g.y(), 640, 360) }
                        }
                    };
                    hs.picked(Some(p), Purpose::Record, 0);
                    let hs2 = hs.clone();
                    glib::timeout_add_local_once(Duration::from_secs(secs), move || {
                        let rec = hs2.recorder.borrow().clone();
                        match rec {
                            Some(r) => r.stop(),
                            None => println!("not recording!"),
                        }
                    });
                    let hs3 = hs.clone();
                    glib::timeout_add_local_once(Duration::from_secs(secs + 3), move || {
                        let t = hs3.thumb.borrow().clone();
                        if let Some(t) = t {
                            println!("thumbnail up: {}x{}", t.img_size().0, t.img_size().1);
                            t.close_now();
                        }
                    });
                });
            }
            "__selftest-menu" => {
                self.capture("menu-test", Target::Active, |hs, c| {
                    hs.captured(c.img, None, None);
                    let hs2 = hs.clone();
                    glib::timeout_add_local_once(Duration::from_millis(500), move || {
                        if let Some(t) = hs2.thumb.borrow().clone() {
                            t.test_menu();
                        }
                    });
                });
            }
            "__selftest" if argv.len() > 2 => {
                // Undocumented: drive the editor with synthetic input, export to argv[2].
                match gdk::Texture::from_filename(&argv[1]) {
                    Ok(t) => {
                        let mut img = Img::from_texture(&t, 1.15);
                        img.scale = 1.15;
                        let ed = editor::Editor::open(self, img, None);
                        editor::Editor::selftest(&ed, PathBuf::from(&argv[2]));
                        self.editors.borrow_mut().push(ed);
                    }
                    Err(e) => eprintln!("{e}"),
                }
            }
            other => eprintln!("holyshot: unknown command {other:?}\n{USAGE}"),
        }
    }

    // ---- capturing --------------------------------------------------------
    fn take_early(&self, cmd: &str) -> Option<Result<Capture, String>> {
        EARLY.with(|e| match e.borrow_mut().take() {
            Some((c, h)) if c == cmd => h.join().ok().flatten(),
            _ => None,
        })
    }

    fn capture(self: &Rc<Self>, cmd: &'static str, target: Target, then: impl FnOnce(&Rc<Hs>, Capture) + 'static) {
        let done = move |hs: &Rc<Hs>, r: Result<Capture, String>| match r {
            Ok(c) => then(hs, c),
            Err(e) => util::notify("Screenshot failed", &e),
        };
        if let Some(r) = self.take_early(cmd) {
            util::trace("using early capture");
            done(self, r);
            return;
        }
        // Already running: make sure our own preview isn't in the shot.
        // (Take it out first: close_now() calls back into thumb_closed().)
        let old = self.thumb.borrow_mut().take();
        let had_thumb = old.is_some();
        if let Some(t) = old {
            t.close_now();
        }
        let hold = self.app.hold();
        let me = self.clone();
        glib::spawn_future_local(async move {
            if had_thumb {
                glib::timeout_future(Duration::from_millis(60)).await;
            }
            let r = gio::spawn_blocking(move || {
                let conn = capture::session()?;
                let mut c = capture::grab(&conn, &target)?;
                if let Target::Workspace = target {
                    c.windows = capture::windows(&conn).unwrap_or_default();
                }
                Ok(c)
            })
                .await
                .unwrap_or_else(|_| Err("capture thread panicked".into()));
            done(&me, r);
            drop(hold);
        });
    }

    /// Pressing a shortcut during the self-timer cancels it.
    fn cancel_countdown(&self) -> bool {
        let cd = self.countdown.borrow_mut().take();
        cd.is_some_and(|c| c.cancel())
    }

    fn pick(self: &Rc<Self>, cmd: &'static str, purpose: Purpose, window_mode: bool) {
        if self.cancel_countdown() {
            return;
        }
        // Pressing the shortcut again while picking cancels. (Bind first: the
        // RefMut must be released before close() calls back into picked().)
        let picking = self.picker.borrow_mut().take();
        if let Some(p) = picking {
            p.close();
            return;
        }
        self.capture(cmd, Target::Workspace, move |hs, c| {
            let picker = overlay::Picker::start(hs, c.img, c.windows, purpose, window_mode);
            *hs.picker.borrow_mut() = Some(picker);
        });
    }

    /// `delay`: the self-timer, in seconds.
    pub fn picked(self: &Rc<Self>, result: Option<Picked>, purpose: Purpose, delay: u32) {
        self.picker.borrow_mut().take();
        let Some(p) = result else { return };
        if delay == 0 {
            return self.act(p, purpose, false);
        }
        let (mon, at) = match &p {
            Picked::Area { mon, rect: (x, y, w, h), .. } => (mon.clone(), (x + w / 2, y + h / 2)),
            Picked::Window { win, mon, .. } => (mon.clone(), (win.x + win.w / 2, win.y + win.h / 2)),
        };
        let me = self.clone();
        let cd = countdown::Countdown::start(self, delay, &mon, at, move || {
            me.countdown.borrow_mut().take();
            me.act(p, purpose, true);
        });
        *self.countdown.borrow_mut() = Some(cd);
    }

    /// `live`: after the self-timer, so the frozen picture is stale.
    fn act(self: &Rc<Self>, p: Picked, purpose: Purpose, live: bool) {
        match (p, purpose) {
            (Picked::Area { mon, rect: (x, y, w, h), .. }, Purpose::Shot) if live => {
                self.capture("timed", Target::Area(x, y, w as u32, h as u32), move |hs, c| hs.captured(c.img, None, Some(mon)));
            }
            (Picked::Area { img, mon, .. }, Purpose::Shot) => self.captured(img, None, Some(mon)),
            (Picked::Window { win, img, mon }, Purpose::Shot) => {
                // Ask KWin for the window itself: uncovered, with its shadow.
                let hold = self.app.hold();
                let me = self.clone();
                glib::spawn_future_local(async move {
                    let r = gio::spawn_blocking(move || capture::session().and_then(|c| capture::grab(&c, &Target::Window(win.id))))
                        .await
                        .unwrap_or_else(|_| Err("capture thread panicked".into()));
                    match r {
                        Ok(c) => me.captured(c.img, None, Some(mon)),
                        Err(e) => {
                            util::notify("Window capture failed, saved what was visible", &e);
                            me.captured(img, None, Some(mon));
                        }
                    }
                    drop(hold);
                });
            }
            (p, Purpose::Record) => {
                let (src, frame, mon) = match p {
                    Picked::Area { img, mon, rect: (x, y, w, h) } => {
                        // even pixel sizes: H.264's 4:2:0 needs them for some encoders
                        let s = mon.scale();
                        let even = |v: i32| {
                            [0, 1, -1, 2, -2, 3, -3].into_iter().map(|d| v + d).find(|&n| n > 0 && ((n as f64 * s).round() as i64) % 2 == 0).unwrap_or(v)
                        };
                        (recorder::Source::Region { x, y, w: even(w), h: even(h), scale: s }, img, mon)
                    }
                    Picked::Window { win, img, mon } => (recorder::Source::Window(win.id), img, mon),
                };
                let hold = self.app.hold();
                let me = self.clone();
                glib::spawn_future_local(async move {
                    let encoder = gio::spawn_blocking(recorder::encoder).await.ok().flatten();
                    // let the picker's overlay leave the screen before KWin's first frame
                    glib::timeout_future(Duration::from_millis(60)).await;
                    let r = match encoder {
                        Some(enc) => recorder::Recorder::start(&me, src, frame, mon, enc),
                        None => Err("no working H.264 encoder found (install gstreamer1-plugins-ugly for x264)".into()),
                    };
                    match r {
                        Ok(rec) => *me.recorder.borrow_mut() = Some(rec),
                        Err(e) => util::notify("Couldn't start recording", &e),
                    }
                    drop(hold);
                });
            }
        }
    }

    pub fn recording_done(self: &Rc<Self>, rec: &recorder::Recorder, video: Option<(PathBuf, Option<Img>, gdk::Monitor, u64)>) {
        {
            let mut cur = self.recorder.borrow_mut();
            if cur.as_ref().is_some_and(|c| std::ptr::eq(c.as_ref(), rec)) {
                cur.take();
            }
        }
        let Some((path, frame, mon, secs)) = video else { return };
        let Some(frame) = frame else { return };
        let want = thumbnail::Thumb::preview_size(&frame, mon.scale());
        let preview = frame.downscale(want.0, want.1);
        let old = self.thumb.borrow_mut().take();
        if let Some(t) = old {
            t.close_now();
        }
        let t = thumbnail::Thumb::show(self, preview, (frame.w, frame.h), path, Some(mon), Some(secs));
        *self.thumb.borrow_mut() = Some(t);
    }

    fn screen(self: &Rc<Self>) {
        let picking = self.picker.borrow_mut().take();
        if let Some(p) = picking {
            p.close();
        }
        self.capture("screen", Target::Active, |hs, c| {
            let mon = util::monitors().into_iter().find(|m| m.connector().as_deref() == c.screen.as_deref());
            hs.captured(c.img, c.preview, mon);
        });
    }

    fn captured(self: &Rc<Self>, img: Img, preview: Option<Img>, mon: Option<gdk::Monitor>) {
        let path = util::unique_path(&util::config().save_dir, "Screenshot", "png");
        self.saving.borrow_mut().insert(path.clone());
        let hold = self.app.hold();
        let me = self.clone();
        // The preview gets a small copy; the full image goes to the save
        // thread as its only owner, so it can be encoded in place.
        let dpr = mon.as_ref().map(|m| m.scale()).unwrap_or(img.scale);
        let want = thumbnail::Thumb::preview_size(&img, dpr);
        let preview = preview.filter(|p| (p.w, p.h) == want).unwrap_or_else(|| img.downscale(want.0, want.1));
        let size = (img.w, img.h);
        let (path2, thumb_path) = (path.clone(), path.clone());
        glib::spawn_future_local(async move {
            let r = gio::spawn_blocking(move || {
                util::trace("save: start");
                img.save_png(&path2)?;
                util::trace("save: done");
                if util::config().copy_to_clipboard {
                    util::clipboard_png_file(&path2);
                }
                Ok::<_, String>(())
            })
            .await
            .unwrap_or_else(|_| Err("save thread panicked".into()));
            me.saving.borrow_mut().remove(&path);
            if let Err(e) = r {
                util::notify("Couldn't save screenshot", &e);
            }
            drop(hold);
        });
        let old = self.thumb.borrow_mut().take();
        if let Some(t) = old {
            t.close_now();
        }
        let t = thumbnail::Thumb::show(self, preview, size, thumb_path, mon, None);
        *self.thumb.borrow_mut() = Some(t);
    }

    pub fn thumb_closed(&self, t: &thumbnail::Thumb) {
        let mut cur = self.thumb.borrow_mut();
        if cur.as_ref().is_some_and(|c| std::ptr::eq(c.as_ref(), t)) {
            cur.take();
        }
    }

    // ---- editor -------------------------------------------------------------
    pub fn open_editor(self: &Rc<Self>, img: Img, path: Option<PathBuf>) {
        let ed = editor::Editor::open(self, img, path);
        self.editors.borrow_mut().push(ed);
    }

    pub fn editor_closed(&self, ed: &editor::Editor) {
        self.editors.borrow_mut().retain(|e| !std::ptr::eq(e.as_ref(), ed));
    }

    fn edit_file(self: &Rc<Self>, path: &std::path::Path) {
        self.edit_file_at_scale(path, 1.0);
    }

    /// `scale`: the screen scale the image was captured at (sets annotation sizes).
    pub fn edit_file_at_scale(self: &Rc<Self>, path: &std::path::Path, scale: f64) {
        match gdk::Texture::from_filename(path) {
            Ok(tex) => self.open_editor(Img::from_texture(&tex, scale), Some(path.to_path_buf())),
            Err(e) => util::notify("Couldn't open image", &format!("{}: {e}", path.display())),
        }
    }

    /// Resolves once a background save of `path` (if any) has finished.
    pub async fn wait_saved(&self, path: &std::path::Path) {
        while self.saving.borrow().contains(path) {
            glib::timeout_future(Duration::from_millis(15)).await;
        }
    }
}
