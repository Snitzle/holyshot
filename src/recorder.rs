//! Screen recording (Mac's Cmd+Shift+5).
//!
//! KWin streams the region or window into PipeWire when asked through its
//! private `zkde_screencast_unstable_v1` Wayland protocol (the one Spectacle
//! uses; install.sh grants it via `X-KDE-Wayland-Interfaces`). We speak the
//! handful of messages it takes straight over the Wayland socket, which
//! needs no Wayland crates, and a `gst-launch-1.0` child encodes the stream:
//! on the GPU when there is one (NVENC takes the stream's BGRx as is, so the
//! CPU never touches a pixel), else x264. It writes Matroska, which stays
//! playable however the recording ends (KWin dropping the stream when a
//! window closes makes the encoder exit without finishing an MP4); a stream
//! copy then turns it into the MP4 everything else expects.
//!
//! A small pill shows the time and stops the recording; pressing the record
//! shortcut again stops it too.

use gtk::{gdk, gio, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::draw::DrawArea;
use crate::img::Img;
use crate::util::{self, rect, rgba};
use crate::Hs;

pub enum Source {
    /// Global logical coordinates, and the scale to record at.
    Region { x: i32, y: i32, w: i32, h: i32, scale: f64 },
    /// KWin's internal window id.
    Window(String),
}

// ---- the Wayland side ---------------------------------------------------------

const POINTER_EMBEDDED: u32 = 2;
const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
const SYNC: u32 = 3;
const SCREENCAST: u32 = 4;
const STREAM: u32 = 5;

/// A bare Wayland client connection holding one screencast stream. KWin ends
/// the stream when this connection closes, so it lives as long as the recording.
pub struct Wl {
    sock: UnixStream,
    buf: Vec<u8>,
}

enum Event {
    Created { node: u32 },
    Serial(u64),
    Closed,
    Failed(String),
    Other,
}

fn arg_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32 + 1).to_ne_bytes());
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn read_str(b: &[u8]) -> String {
    let n = b.get(..4).map(|l| u32::from_ne_bytes(l.try_into().unwrap()) as usize).unwrap_or(0);
    b.get(4..4 + n.saturating_sub(1)).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default()
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    b.get(i * 4..i * 4 + 4).map(|s| u32::from_ne_bytes(s.try_into().unwrap())).unwrap_or(0)
}

impl Wl {
    fn send(&mut self, obj: u32, op: u16, args: &[u8]) -> std::io::Result<()> {
        let mut m = Vec::with_capacity(8 + args.len());
        m.extend_from_slice(&obj.to_ne_bytes());
        m.extend_from_slice(&((((8 + args.len()) as u32) << 16) | op as u32).to_ne_bytes());
        m.extend_from_slice(args);
        self.sock.write_all(&m)
    }

    /// Next complete message from what's buffered: (object, opcode, args).
    fn take(&mut self) -> Option<(u32, u16, Vec<u8>)> {
        if self.buf.len() < 8 {
            return None;
        }
        let obj = u32_at(&self.buf, 0);
        let w = u32_at(&self.buf, 1);
        let size = (w >> 16) as usize;
        if size < 8 || self.buf.len() < size {
            return None;
        }
        let args = self.buf[8..size].to_vec();
        self.buf.drain(..size);
        Some((obj, (w & 0xffff) as u16, args))
    }

    fn fill(&mut self) -> std::io::Result<usize> {
        let mut chunk = [0u8; 4096];
        let n = self.sock.read(&mut chunk)?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n)
    }

    fn next_blocking(&mut self) -> Result<(u32, u16, Vec<u8>), String> {
        loop {
            if let Some(m) = self.take() {
                if m.0 == DISPLAY && m.1 == 0 {
                    return Err(format!("Wayland error: {}", read_str(&m.2[8..])));
                }
                return Ok(m);
            }
            match self.fill() {
                Ok(0) => return Err("KWin closed the connection".into()),
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    fn event(m: &(u32, u16, Vec<u8>)) -> Event {
        match m {
            (STREAM, 0, _) => Event::Closed,
            (STREAM, 1, a) => Event::Created { node: u32_at(a, 0) },
            (STREAM, 2, a) => Event::Failed(read_str(a)),
            (STREAM, 3, a) => Event::Serial(((u32_at(a, 0) as u64) << 32) | u32_at(a, 1) as u64),
            _ => Event::Other,
        }
    }

    /// Connects and asks KWin for the stream. Returns the PipeWire target
    /// (object serial, or node id on older KWin) for `pipewiresrc`.
    pub fn start(src: &Source) -> Result<(Wl, String), String> {
        let name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
        let path = if name.starts_with('/') { PathBuf::from(name) } else { glib::user_runtime_dir().join(name) };
        let sock = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut wl = Wl { sock, buf: Vec::new() };
        let io = |e: std::io::Error| e.to_string();
        wl.send(DISPLAY, 1, &REGISTRY.to_ne_bytes()).map_err(io)?; // get_registry
        wl.send(DISPLAY, 0, &SYNC.to_ne_bytes()).map_err(io)?; // sync: marks the end of the globals
        let mut global = None;
        loop {
            let (obj, op, a) = wl.next_blocking()?;
            match (obj, op) {
                (REGISTRY, 0) if read_str(&a[4..]) == "zkde_screencast_unstable_v1" => {
                    let iface_len = (u32_at(&a, 1) as usize).div_ceil(4) * 4;
                    global = Some((u32_at(&a, 0), u32_at(&a[8 + iface_len..], 0)));
                }
                (SYNC, 0) => break,
                _ => {}
            }
        }
        // Restricted globals are simply not shown to unauthorised clients.
        let (gname, version) = global.ok_or(
            "KWin didn't allow screen recording. Re-run install.sh so the desktop file grants zkde_screencast_unstable_v1.",
        )?;
        if version < 3 {
            return Err("this KWin is too old for region recording".into());
        }
        let version = version.min(6);
        let mut a = gname.to_ne_bytes().to_vec();
        arg_str(&mut a, "zkde_screencast_unstable_v1");
        a.extend_from_slice(&version.to_ne_bytes());
        a.extend_from_slice(&SCREENCAST.to_ne_bytes());
        wl.send(REGISTRY, 0, &a).map_err(io)?; // bind

        let mut a = STREAM.to_ne_bytes().to_vec();
        match src {
            Source::Region { x, y, w, h, scale } => {
                // From v5, scale 0 means "the highest scale of the screens it covers".
                let scale = if version >= 5 { 0.0 } else { *scale };
                for v in [*x, *y] {
                    a.extend_from_slice(&v.to_ne_bytes());
                }
                for v in [*w as u32, *h as u32] {
                    a.extend_from_slice(&v.to_ne_bytes());
                }
                a.extend_from_slice(&((scale * 256.0).round() as i32).to_ne_bytes());
                a.extend_from_slice(&POINTER_EMBEDDED.to_ne_bytes());
                wl.send(SCREENCAST, 4, &a).map_err(io)?; // stream_region (destroy is 2, stream_virtual_output 3)
            }
            Source::Window(id) => {
                arg_str(&mut a, id);
                a.extend_from_slice(&POINTER_EMBEDDED.to_ne_bytes());
                wl.send(SCREENCAST, 1, &a).map_err(io)?; // stream_window
            }
        }
        let mut serial = None;
        loop {
            let m = wl.next_blocking()?;
            match Wl::event(&m) {
                Event::Serial(s) => serial = Some(s),
                Event::Created { node } => {
                    let target = match serial {
                        Some(s) => format!("target-object={s}"),
                        None => format!("path={node}"),
                    };
                    wl.sock.set_read_timeout(None).ok();
                    return Ok((wl, target));
                }
                Event::Failed(e) => return Err(e),
                Event::Closed => return Err("KWin closed the stream".into()),
                Event::Other => {}
            }
        }
    }

    /// A second handle on the connection that blocks (on a worker thread)
    /// until KWin ends the stream or the connection is shut down.
    fn watcher(&mut self) -> std::io::Result<impl FnOnce() + Send + 'static> {
        let mut r = Wl { sock: self.sock.try_clone()?, buf: std::mem::take(&mut self.buf) };
        Ok(move || loop {
            while let Some(m) = r.take() {
                if (m.0 == DISPLAY && m.1 == 0) || matches!(Wl::event(&m), Event::Closed | Event::Failed(_)) {
                    util::trace(&format!("recorder: KWin ended the stream ({}#{} {})", m.0, m.1, read_str(&m.2)));
                    return;
                }
            }
            if !matches!(r.fill(), Ok(n) if n > 0) {
                util::trace("recorder: connection closed");
                return;
            }
        })
    }

    fn close(mut self) {
        let _ = self.send(STREAM, 0, &[]); // close
        let _ = self.send(SCREENCAST, 2, &[]); // destroy
        let _ = self.sock.shutdown(std::net::Shutdown::Both); // wakes the watcher
    }
}

// ---- the encoder ------------------------------------------------------------------

/// (gst element, pipeline fragment from raw video to H.264), best first.
/// No B-frames or lookahead: each frame is written as soon as it's encoded,
/// so nothing is lost when KWin ends the stream abruptly (the encoder then
/// exits on an error, and frames it was holding back would be dropped).
const ENCODERS: [(&str, &str); 5] = [
    ("nvh264enc", "nvh264enc preset=p4 rc-mode=vbr const-quality=26 zerolatency=true bframes=0 rc-lookahead=0"),
    ("vah264lpenc", "videoconvert ! vah264lpenc b-frames=0"),
    ("vah264enc", "videoconvert ! vah264enc b-frames=0"),
    ("x264enc", "videoconvert ! video/x-raw,format=I420 ! x264enc speed-preset=veryfast tune=zerolatency pass=qual quantizer=23"),
    ("openh264enc", "videoconvert ! openh264enc complexity=low"),
];

/// The first encoder that actually works here (a plugin can exist without
/// its GPU). Probing costs ~0.6 s for NVENC, so the answer is kept until
/// reboot in the runtime dir; it runs while the user is still picking.
pub fn encoder() -> Option<&'static str> {
    static E: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *E.get_or_init(pick_encoder)
}

fn pick_encoder() -> Option<&'static str> {
    let cache = glib::user_runtime_dir().join("holyshot").join("encoder");
    let want = util::config().record_encoder.clone();
    if want != "auto" {
        return ENCODERS.iter().find(|(n, _)| *n == want).map(|(_, p)| *p);
    }
    if let Ok(name) = std::fs::read_to_string(&cache) {
        if let Some((_, p)) = ENCODERS.iter().find(|(n, _)| *n == name.trim()) {
            return Some(p);
        }
    }
    for (name, pipe) in ENCODERS {
        let ok = util::command("gst-launch-1.0")
            .args(["-q", "videotestsrc", "num-buffers=2", "!", "video/x-raw,format=BGRx,width=320,height=180", "!", "videoconvert", "!"])
            .args(pipe.split(' '))
            .args(["!", "fakesink"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            let _ = std::fs::create_dir_all(cache.parent().unwrap());
            let _ = std::fs::write(&cache, name);
            util::trace("recorder: encoder probed");
            return Some(pipe);
        }
    }
    None
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn prctl(option: i32, arg2: u64, ...) -> i32;
}
const SIGINT: i32 = 2;
const PR_SET_PDEATHSIG: i32 = 1;

// ---- the recording ------------------------------------------------------------------

pub struct Recorder {
    hs: Weak<Hs>,
    wl: RefCell<Option<Wl>>,
    pid: Cell<Option<i32>>,
    path: PathBuf,
    tmp: PathBuf,
    log: PathBuf,
    preview: RefCell<Option<Img>>,
    mon: gdk::Monitor,
    pill: gtk::Window,
    area: DrawArea,
    started: Instant,
    seconds: Cell<Option<u64>>,
    stopping: Cell<bool>,
    sources: RefCell<Vec<glib::SourceId>>,
    copier: RefCell<Option<std::thread::JoinHandle<()>>>,
    _hold: gio::ApplicationHoldGuard,
}

impl Recorder {
    /// `frame`: the frozen pixels of what's being recorded (for the preview
    /// afterwards). `encoder`: from `encoder()`.
    pub fn start(hs: &Rc<Hs>, src: Source, frame: Img, mon: gdk::Monitor, encoder: &str) -> Result<Rc<Recorder>, String> {
        let pill_rect = pill_spot(&src, &mon);
        let (wl, target) = Wl::start(&src)?;
        util::trace("recorder: stream created");
        let path = util::unique_path(&util::config().record_dir, "Screen Recording", "mp4");
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let tmp = path.with_file_name(format!(".{stem}.mkv"));
        let log = glib::user_runtime_dir().join("holyshot").join(format!("record-{}.log", std::process::id()));
        let _ = std::fs::create_dir_all(log.parent().unwrap());
        let pipeline = format!(
            "pipewiresrc {target} do-timestamp=true keepalive-time=1000 on-disconnect=eos ! \
             video/x-raw,max-framerate=60/1 ! videoconvert ! {encoder} ! h264parse ! matroskamux streamable=true ! fdsink fd=1"
        );
        // Not filesink: when KWin drops the stream, pipewiresrc flushes on its
        // way down, and filesink answers a flush by truncating the file. A pipe
        // can't be truncated; a thread copies it into the file.
        let mut out = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let mut cmd = util::command("gst-launch-1.0");
        cmd.args(["-q", "-e"]).args(pipeline.split(' ').filter(|s| !s.is_empty()));
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::fs::File::create(&log).map(std::process::Stdio::from).unwrap_or_else(|_| std::process::Stdio::null()));
        // If we die, the encoder gets SIGINT, which (with -e) finishes the file.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                prctl(PR_SET_PDEATHSIG, SIGINT as u64);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("gst-launch-1.0: {e}"))?;
        let pid = child.id() as i32;
        let mut pipe = child.stdout.take().unwrap();
        std::mem::forget(child); // reaped by the glib child watch below
        let copier = std::thread::spawn(move || {
            let _ = std::io::copy(&mut pipe, &mut out);
        });

        let pill = gtk::Window::new();
        pill.set_application(Some(&hs.app));
        pill.init_layer_shell();
        pill.set_layer(Layer::Overlay);
        pill.set_namespace(Some(util::LAYER_NAMESPACE));
        pill.set_monitor(Some(&mon));
        pill.set_anchor(Edge::Top, true);
        pill.set_anchor(Edge::Left, true);
        pill.set_margin(Edge::Left, pill_rect.0);
        pill.set_margin(Edge::Top, pill_rect.1);
        pill.set_keyboard_mode(KeyboardMode::None);
        pill.set_decorated(false);
        pill.add_css_class("hs-layer");
        pill.set_default_size(PILL_W, PILL_H);
        let area = DrawArea::new();
        area.set_size_request(PILL_W, PILL_H);
        area.set_cursor_from_name(Some("pointer"));
        pill.set_child(Some(&area));

        let rec = Rc::new(Recorder {
            hs: Rc::downgrade(hs),
            wl: RefCell::new(Some(wl)),
            pid: Cell::new(Some(pid)),
            path,
            tmp,
            log,
            preview: RefCell::new(Some(frame)),
            mon,
            pill,
            area,
            started: Instant::now(),
            seconds: Cell::new(None),
            stopping: Cell::new(false),
            sources: RefCell::default(),
            copier: RefCell::new(Some(copier)),
            _hold: hs.app.hold(),
        });
        rec.connect(pid);
        rec.pill.present();
        Ok(rec)
    }

    fn connect(self: &Rc<Self>, pid: i32) {
        let w = Rc::downgrade(self);
        self.area.set_draw(move |widget, sn, _, _| {
            if let Some(r) = w.upgrade() {
                r.draw(widget, sn);
            }
        });
        let click = gtk::GestureClick::new();
        let w = Rc::downgrade(self);
        click.connect_released(move |_, _, _, _| {
            if let Some(r) = w.upgrade() {
                r.stop();
            }
        });
        self.area.add_controller(click);

        // the clock: one tiny repaint a second
        let w = Rc::downgrade(self);
        let tick = glib::timeout_add_seconds_local(1, move || match w.upgrade() {
            Some(r) => {
                r.area.queue_draw();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        // KWin ends the stream if the window closes or the screen changes.
        let watcher = self.wl.borrow_mut().as_mut().map(|wl| wl.watcher());
        if let Some(Ok(watcher)) = watcher {
            let w = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                let _ = gio::spawn_blocking(watcher).await;
                if let Some(r) = w.upgrade() {
                    r.stop();
                }
            });
        }
        let w = Rc::downgrade(self);
        let child = glib::child_watch_add_local(glib::Pid(pid), move |_, status| {
            if let Some(r) = w.upgrade() {
                r.pid.set(None);
                r.finished(status == 0);
            }
        });
        self.sources.borrow_mut().extend([tick, child]);
    }

    /// Ends the recording; the preview appears once the file is complete.
    pub fn stop(self: &Rc<Self>) {
        if self.stopping.replace(true) {
            return;
        }
        self.seconds.set(Some(self.started.elapsed().as_secs_f64().round() as u64));
        util::trace("recorder: stop");
        self.area.queue_draw();
        match self.pid.get() {
            Some(pid) => unsafe {
                kill(pid, SIGINT); // -e: send EOS, so mp4mux writes its index
            },
            None => self.finished(false),
        }
    }

    /// The encoder has exited (`ok`: cleanly): remux to MP4, show the preview.
    fn finished(self: &Rc<Self>, ok: bool) {
        self.stopping.set(true);
        self.area.queue_draw();
        if self.seconds.get().is_none() {
            self.seconds.set(Some(self.started.elapsed().as_secs_f64().round() as u64));
        }
        for id in self.sources.borrow_mut().drain(..) {
            util::remove_source(id);
        }
        if let Some(wl) = self.wl.borrow_mut().take() {
            wl.close();
        }
        if !ok {
            util::trace("recorder: encoder exited with an error (stream ended?), keeping what was recorded");
        }
        if util::tracing() {
            eprint!("{}", std::fs::read_to_string(&self.log).unwrap_or_default());
        }
        let me = self.clone();
        glib::spawn_future_local(async move {
            let (tmp, path) = (me.tmp.clone(), me.path.clone());
            let copier = me.copier.borrow_mut().take();
            let saved = gio::spawn_blocking(move || {
                if let Some(c) = copier {
                    let _ = c.join(); // the last bytes
                }
                remux(&tmp, &path)
            })
            .await
            .unwrap_or(None);
            util::trace("recorder: saved");
            if saved.is_none() {
                let err = std::fs::read_to_string(&me.log).unwrap_or_default();
                let err = err.lines().filter(|l| l.contains("ERROR")).last().unwrap_or("nothing was recorded").to_string();
                util::notify("Recording failed", &err);
            }
            let _ = std::fs::remove_file(&me.log);
            me.pill.destroy();
            let Some(hs) = me.hs.upgrade() else { return };
            let preview = me.preview.borrow_mut().take();
            let secs = me.seconds.get().unwrap_or(0);
            hs.recording_done(&me, saved.map(|p| (p, preview, me.mon.clone(), secs)));
        });
    }

    fn draw(&self, widget: &DrawArea, sn: &gtk::Snapshot) {
        let (w, h) = (PILL_W as f32, PILL_H as f32);
        let r = rect(0.0, 0.0, w, h);
        let rr = util::rounded(r, h / 2.0);
        sn.push_rounded_clip(&rr);
        sn.append_color(&rgba(0.1, 0.1, 0.1, 0.92), &r);
        sn.pop();
        sn.append_border(&rr, &[1.0; 4], &[rgba(1.0, 1.0, 1.0, 0.2); 4]);
        let stopping = self.stopping.get();
        // red dot (or grey while the file is being finished)
        let dot = rect(12.0, h / 2.0 - 5.0, 10.0, 10.0);
        sn.push_rounded_clip(&util::rounded(dot, 5.0));
        sn.append_color(&if stopping { rgba(0.6, 0.6, 0.6, 1.0) } else { rgba(1.0, 0.25, 0.22, 1.0) }, &dot);
        sn.pop();
        let s = self.started.elapsed().as_secs();
        let text = if stopping { "Saving…".to_string() } else { format!("{}:{:02}", s / 60, s % 60) };
        let l = util::layout(widget, &text, 10.0, true);
        let (_, lh) = l.pixel_size();
        sn.save();
        sn.translate(&gtk::graphene::Point::new(30.0, (h - lh as f32) / 2.0));
        sn.append_layout(&l, &gdk::RGBA::WHITE);
        sn.restore();
        if !stopping {
            // stop square
            let sq = rect(w - 28.0, h / 2.0 - 6.0, 12.0, 12.0);
            sn.push_rounded_clip(&util::rounded(sq, 2.0));
            sn.append_color(&gdk::RGBA::WHITE, &sq);
            sn.pop();
        }
    }
}

/// Matroska -> MP4 without re-encoding (`faststart`: index first, so it can
/// play while still downloading). Returns the file the user gets: the MP4,
/// or the Matroska file itself if that fails.
fn remux(tmp: &std::path::Path, mp4: &std::path::Path) -> Option<PathBuf> {
    if !std::fs::metadata(tmp).is_ok_and(|m| m.len() > 0) {
        let _ = std::fs::remove_file(tmp);
        return None;
    }
    let ok = util::command("gst-launch-1.0")
        .args(["-q", "filesrc", &format!("location={}", tmp.display()), "!", "matroskademux", "!", "h264parse", "!"])
        .args(["mp4mux", "faststart=true", "!", "filesink", &format!("location={}", mp4.display())])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
        && std::fs::metadata(mp4).is_ok_and(|m| m.len() > 0);
    if ok {
        let _ = std::fs::remove_file(tmp);
        return Some(mp4.to_path_buf());
    }
    let _ = std::fs::remove_file(mp4);
    let mkv = mp4.with_extension("mkv");
    std::fs::rename(tmp, &mkv).ok().map(|_| mkv)
}

const PILL_W: i32 = 104;
const PILL_H: i32 = 30;

/// Where the pill goes (monitor-local top-left): beside the recorded area
/// rather than over it when there's room, so it stays out of the video.
fn pill_spot(src: &Source, mon: &gdk::Monitor) -> (i32, i32) {
    let g = mon.geometry();
    let (mw, mh) = (g.width(), g.height());
    let (pw, ph, gap) = (PILL_W, PILL_H, 8);
    let top_center = ((mw - pw) / 2, gap);
    let Source::Region { x, y, w, h, .. } = *src else { return top_center };
    let (x, y) = (x - g.x(), y - g.y());
    let fits = |(px, py): (i32, i32)| {
        px >= 0 && py >= 0 && px + pw <= mw && py + ph <= mh && (px + pw <= x || px >= x + w || py + ph <= y || py >= y + h)
    };
    let cx = (x + (w - pw) / 2).clamp(gap, (mw - pw - gap).max(gap));
    [(cx, y + h + gap), (cx, y - ph - gap), top_center, ((mw - pw) / 2, mh - ph - gap), (mw - pw - gap, gap)]
        .into_iter()
        .find(|&p| fits(p))
        .unwrap_or((mw - pw - gap, gap))
}
