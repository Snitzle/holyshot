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

pub const APP_ID: &str = "dev.holyshot";

const USAGE: &str = "\
holyshot [COMMAND]
  area         pick an area of the screen (default)
  screen       capture the whole screen the mouse is on
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
    if cmd == "area" {
        // The picker's first frame draws text; fontconfig's one-time set-up
        // (~19 ms, process-global and thread-safe) would otherwise run on the
        // main thread right then. Do it now, in parallel with GTK start-up.
        std::thread::spawn(|| {
            util::warm_fontconfig();
            util::trace("fontconfig warm");
        });
    }
    if matches!(cmd, "area" | "screen") {
        let target = if cmd == "area" { Target::Workspace } else { Target::Active };
        let handle = std::thread::spawn(move || {
            let conn = capture::session().ok()?;
            if capture::other_instance_running(&conn) {
                return None;
            }
            Some(capture::grab(&conn, &target))
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
            saving: RefCell::default(),
        })
    }

    fn handle(self: &Rc<Self>, argv: &[String], cwd: Option<PathBuf>) {
        match argv.first().map(String::as_str).unwrap_or("area") {
            "area" => self.area(),
            "screen" => self.screen(),
            "edit" => {
                for f in &argv[1..] {
                    let p = PathBuf::from(f);
                    let p = if p.is_absolute() { p } else { cwd.clone().unwrap_or_default().join(p) };
                    self.edit_file(&p);
                }
            }
            "__selftest-area" => {
                self.capture("area-test", Target::Workspace, |hs, c| {
                    let picker = overlay::Picker::start(hs, c.img);
                    overlay::selftest(&picker);
                    *hs.picker.borrow_mut() = Some(picker);
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
            "__selftest-menu" => {
                self.capture("menu-test", Target::Active, |hs, c| {
                    hs.captured(c.img, None);
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
            let r = gio::spawn_blocking(move || capture::session().and_then(|c| capture::grab(&c, &target)))
                .await
                .unwrap_or_else(|_| Err("capture thread panicked".into()));
            done(&me, r);
            drop(hold);
        });
    }

    fn area(self: &Rc<Self>) {
        // Pressing the shortcut again while picking cancels. (Bind first: the
        // RefMut must be released before close() calls back into picked().)
        let picking = self.picker.borrow_mut().take();
        if let Some(p) = picking {
            p.close();
            return;
        }
        self.capture("area", Target::Workspace, |hs, c| {
            let picker = overlay::Picker::start(hs, c.img);
            *hs.picker.borrow_mut() = Some(picker);
        });
    }

    pub fn picked(self: &Rc<Self>, result: Option<(Img, gdk::Monitor)>) {
        self.picker.borrow_mut().take();
        if let Some((img, mon)) = result {
            self.captured(img, Some(mon));
        }
    }

    fn screen(self: &Rc<Self>) {
        let picking = self.picker.borrow_mut().take();
        if let Some(p) = picking {
            p.close();
        }
        self.capture("screen", Target::Active, |hs, c| {
            let mon = util::monitors().into_iter().find(|m| m.connector().as_deref() == c.screen.as_deref());
            hs.captured(c.img, mon);
        });
    }

    fn captured(self: &Rc<Self>, img: Img, mon: Option<gdk::Monitor>) {
        let path = util::unique_path();
        self.saving.borrow_mut().insert(path.clone());
        let hold = self.app.hold();
        let me = self.clone();
        let (img2, path2, thumb_path) = (img.clone(), path.clone(), path.clone());
        glib::spawn_future_local(async move {
            let r = gio::spawn_blocking(move || {
                util::trace("save: start");
                img2.save_png(&path2)?;
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
        let t = thumbnail::Thumb::show(self, img, thumb_path, mon);
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
        match gdk::Texture::from_filename(path) {
            Ok(tex) => self.open_editor(Img::from_texture(&tex, 1.0), Some(path.to_path_buf())),
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
