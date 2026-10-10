//! View ▸ Stereo Monitor, driven headless through the control channel with a recording host in
//! place of the native window: the command is offered for a stereo sequence on a host that has a
//! window; it hands the host the Program monitor's frame as one stereo picture (full side by
//! side, one eye per half), nothing while it is off, and it follows the window being closed.
//! The picture of a sequence that is not stereo is passed as it is, marked as not stereo (the
//! window never declares it).

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use filmcraft_ui_egui::stereo_monitor::{StereoFrame, StereoMonitorHost};
use serde_json::{Value, json};

#[derive(Default)]
struct Log {
    /// (width, height, buffer length, stereo) of every picture shown
    shown: Vec<(usize, usize, usize, bool)>,
    open: bool,
    closes: usize,
}

struct Recorder(Arc<Mutex<Log>>);

impl StereoMonitorHost for Recorder {
    fn show(&mut self, frame: StereoFrame) -> Result<(), String> {
        let mut log = self.0.lock().unwrap();
        log.shown.push((frame.width, frame.height, frame.rgba.len(), frame.stereo));
        log.open = true;
        Ok(())
    }
    fn is_open(&self) -> bool {
        self.0.lock().unwrap().open
    }
    fn close(&mut self) {
        let mut log = self.0.lock().unwrap();
        log.open = false;
        log.closes += 1;
    }
}

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    log: Arc<Mutex<Log>>,
}

impl Driver {
    fn new(with_window: bool) -> Self {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        let (tx, rx) = channel();
        let mut app = FilmcraftApp::new(session).with_control(rx);
        let log = Arc::new(Mutex::new(Log::default()));
        if with_window {
            app.hooks.stereo_monitor = Some(Box::new(Recorder(log.clone())));
        }
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, log };
        d.frames(4);
        d
    }
    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                return v;
            }
        }
        panic!("no reply to {method} {params}");
    }
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.call(method, params.clone());
        assert_eq!(r["ok"], true, "{method} {params}: {r}");
        r["result"].clone()
    }
    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }
    fn menu_item(&mut self) -> Value {
        let menu = self.ok("ui.menu.list", json!({}));
        menu.as_array().unwrap().iter().find(|m| m["id"] == "view.stereoMonitor").cloned().expect("View > Stereo Monitor")
    }
    /// Steps the UI until the host has shown `n` pictures (the frame workers deliver them).
    fn until_shown(&mut self, n: usize) {
        for _ in 0..1500 {
            if self.log.lock().unwrap().shown.len() >= n {
                return;
            }
            self.frames(2);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the host got {} pictures, wanted {n}", self.log.lock().unwrap().shown.len());
    }
}

#[test]
fn the_stereo_monitor_is_offered_for_a_stereo_sequence_only() {
    let mut d = Driver::new(true);
    let item = d.menu_item();
    assert_eq!(item["path"], json!(["View"]));
    assert_eq!(item["enabled"], false, "the demo sequence is 2D");
    let r = d.call("ui.menu.invoke", json!({"id": "view.stereoMonitor"}));
    assert_eq!(r["ok"], false, "a 2D sequence has no stereo picture to show: {r}");
    d.frames(10);
    assert!(d.log.lock().unwrap().shown.is_empty(), "nothing is shown while it is off");

    d.exec("file.newSequence", json!({"name": "Stereo", "width": 128, "height": 36, "stereo": true}));
    d.frames(3);
    assert_eq!(d.menu_item()["enabled"], true);
    assert_eq!(d.menu_item()["checked"], false);
}

#[test]
fn a_host_without_a_window_leaves_the_command_off() {
    let mut d = Driver::new(false);
    d.exec("file.newSequence", json!({"name": "Stereo", "width": 128, "height": 36, "stereo": true}));
    d.frames(3);
    assert_eq!(d.menu_item()["enabled"], false);
    let r = d.call("ui.menu.invoke", json!({"id": "view.stereoMonitor"}));
    assert_eq!(r["ok"], false, "{r}");
}

#[test]
fn the_window_gets_the_frame_as_one_stereo_picture_while_it_is_on() {
    let mut d = Driver::new(true);
    d.exec("file.newSequence", json!({"name": "Stereo", "width": 128, "height": 36, "stereo": true}));
    d.frames(3);
    d.ok("ui.menu.invoke", json!({"id": "view.stereoMonitor"}));
    assert_eq!(d.menu_item()["checked"], true);
    d.until_shown(1);
    // the sequence's own size: full side by side, 64 x 36 per eye, RGBA
    assert_eq!(d.log.lock().unwrap().shown[0], (128, 36, 128 * 36 * 4, true));

    // off: the host closes the window and gets nothing more
    d.ok("ui.menu.invoke", json!({"id": "view.stereoMonitor"}));
    d.frames(6);
    let (n, closes) = {
        let log = d.log.lock().unwrap();
        (log.shown.len(), log.closes)
    };
    assert_eq!(closes, 1);
    d.frames(30);
    assert_eq!(d.log.lock().unwrap().shown.len(), n, "nothing is shown while it is off");
}

#[test]
fn closing_the_window_turns_the_command_off() {
    let mut d = Driver::new(true);
    d.exec("file.newSequence", json!({"name": "Stereo", "width": 128, "height": 36, "stereo": true}));
    d.frames(3);
    d.ok("ui.menu.invoke", json!({"id": "view.stereoMonitor"}));
    d.until_shown(1);
    // the user closes the window
    d.log.lock().unwrap().open = false;
    d.frames(6);
    assert_eq!(d.menu_item()["checked"], false);
    let n = d.log.lock().unwrap().shown.len();
    d.frames(30);
    assert_eq!(d.log.lock().unwrap().shown.len(), n, "a closed window gets no pictures");
}

#[test]
fn a_sequence_that_is_not_stereo_is_passed_as_not_stereo() {
    let mut d = Driver::new(true);
    d.exec("file.newSequence", json!({"name": "Stereo", "width": 128, "height": 36, "stereo": true}));
    d.frames(3);
    d.ok("ui.menu.invoke", json!({"id": "view.stereoMonitor"}));
    d.until_shown(1);
    assert!(d.log.lock().unwrap().shown[0].3);
    // a new 2D sequence becomes the active one; the window follows it and the picture says 2D,
    // which is what keeps the window from declaring it
    d.exec("file.newSequence", json!({"name": "Flat", "width": 128, "height": 36, "stereo": false}));
    let before = d.log.lock().unwrap().shown.len();
    d.until_shown(before + 1);
    let last = *d.log.lock().unwrap().shown.last().unwrap();
    assert_eq!(last, (128, 36, 128 * 36 * 4, false));
}
