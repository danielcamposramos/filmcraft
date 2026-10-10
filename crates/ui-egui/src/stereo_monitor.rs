//! View ▸ Stereo Monitor: the Program monitor's stereo picture in a window of its own, declared
//! to the desktop.
//!
//! A stereo sequence's frames are full side by side with the left eye first (see
//! `filmcraft_engine::stereo`). The monitor panel shows such a frame as it is, both eyes in a
//! wide picture. The Stereo Monitor hands the same frame to the host's stereo window, which tells
//! the desktop it holds a stereo pair (the desktop then shows it on each screen as that screen
//! can: a 3D mode, or the left eye on a plain screen). The program has no output formats of its
//! own: one format goes out, declared once, and only while a stereo picture is shown.
//!
//! The window belongs to the host ([`StereoMonitorHost`], `HostHooks::stereo_monitor`): the
//! desktop app implements it with `filmcraft-stereo-window`; hosts without one (the web) leave
//! the command disabled.

use std::sync::Arc;

use crate::FilmcraftApp;
use crate::frames::{FrameKey, Target};

/// One picture for the host's stereo window: RGBA8, straight alpha. A stereo picture is full
/// side by side with the left eye first.
pub struct StereoFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Arc<[u8]>,
    /// The picture holds a stereo pair.
    pub stereo: bool,
}

/// A window the host shows the stereo picture in.
pub trait StereoMonitorHost {
    /// Shows the next picture, opening the window with the first one. An error says why it could
    /// not be opened.
    fn show(&mut self, frame: StereoFrame) -> Result<(), String>;
    /// The window is on screen (false once the user closed it).
    fn is_open(&self) -> bool;
    /// Closes the window.
    fn close(&mut self);
}

/// The Stereo Monitor's state.
#[derive(Default)]
pub struct StereoMonitor {
    /// View ▸ Stereo Monitor is on.
    pub wanted: bool,
    /// The host has the window open.
    opened: bool,
    /// The frame the window shows.
    shown: Option<FrameKey>,
}

/// Whether the Program monitor shows a stereo picture: the active sequence is a stereo sequence
/// (full side by side, left eye first).
pub fn shows_stereo(app: &FilmcraftApp) -> bool {
    app.session.state.active_sequence.is_some_and(|i| filmcraft_engine::stereo::item_is_stereo(&app.session.project, i))
}

/// Menu enablement of `view.stereoMonitor`: a host with a window and a stereo picture to show
/// (or the window already on, so it can be turned off).
pub fn enabled(app: &FilmcraftApp) -> bool {
    app.stereo_monitor.wanted || (app.hooks.stereo_monitor.is_some() && shows_stereo(app))
}

/// `view.stereoMonitor`: turns the window on or off.
pub fn toggle(app: &mut FilmcraftApp, on: bool) -> Result<(), String> {
    if on && !app.stereo_monitor.wanted && !enabled(app) {
        return Err(if app.hooks.stereo_monitor.is_none() {
            "this host has no stereo window".into()
        } else {
            "the Program monitor does not show a stereo sequence".into()
        });
    }
    app.stereo_monitor.wanted = on;
    Ok(())
}

/// Once per UI pass: opens, feeds and closes the window.
pub fn tick(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let Some(mut host) = app.hooks.stereo_monitor.take() else {
        app.stereo_monitor.wanted = false;
        return;
    };
    step(app, ctx, host.as_mut());
    app.hooks.stereo_monitor = Some(host);
}

fn step(app: &mut FilmcraftApp, ctx: &egui::Context, host: &mut dyn StereoMonitorHost) {
    if !app.stereo_monitor.wanted {
        if std::mem::take(&mut app.stereo_monitor.opened) {
            host.close();
        }
        app.stereo_monitor.shown = None;
        return;
    }
    if app.stereo_monitor.opened && !host.is_open() {
        // the user closed the window: the menu entry goes off with it
        app.stereo_monitor.wanted = false;
        app.stereo_monitor.opened = false;
        app.stereo_monitor.shown = None;
        return;
    }
    let Some(seq) = app.session.state.active_sequence else { return };
    let Some(q) = app.session.active_sequence() else { return };
    let rate = q.settings.frame_rate;
    let frame = rate.frame_at(app.session.playhead());
    // full size, the exact frame, whatever the monitor panel shows
    let key = FrameKey { target: Target::Sequence(seq), frame, size: 1000, revision: app.session.revision, draft: false };
    if app.stereo_monitor.shown == Some(key) {
        return;
    }
    let project = app.session.project.clone();
    app.frames.request(key, rate.tick_of(frame), 1.0, &project, 0);
    let Some(img) = app.frames.get(&key) else {
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
        return;
    };
    let stereo = shows_stereo(app);
    let picture = StereoFrame { width: img.w, height: img.h, rgba: Arc::from(img.px.as_slice()), stereo };
    match host.show(picture) {
        Ok(()) => {
            app.stereo_monitor.opened = true;
            app.stereo_monitor.shown = Some(key);
        }
        Err(e) => {
            app.stereo_monitor.wanted = false;
            app.stereo_monitor.opened = host.is_open();
            let now = ctx.input(|i| i.time);
            app.toast = Some((format!("Stereo Monitor: {e}"), now));
        }
    }
}
