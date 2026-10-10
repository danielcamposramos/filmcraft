//! View ▸ Stereo Monitor on the desktop: the window `filmcraft-stereo-window` opens, declared to
//! the desktop while it shows a stereo pair.

use filmcraft_stereo_window::{Picture, StereoWindow};
use filmcraft_ui_egui::stereo_monitor::{StereoFrame, StereoMonitorHost};

pub struct Host {
    app_id: &'static str,
    window: Option<StereoWindow>,
}

impl Host {
    pub fn new(app_id: &'static str) -> Self {
        Self { app_id, window: None }
    }
}

impl StereoMonitorHost for Host {
    fn show(&mut self, frame: StereoFrame) -> Result<(), String> {
        let picture = Picture::new(frame.width, frame.height, frame.rgba, frame.stereo).map_err(|e| e.to_string())?;
        if self.window.is_none() {
            self.window = Some(StereoWindow::open("FilmCraft Stereo Monitor", self.app_id).map_err(|e| e.to_string())?);
        }
        match &self.window {
            Some(w) if w.show(picture) => Ok(()),
            _ => Err("the window is closed".into()),
        }
    }

    fn is_open(&self) -> bool {
        self.window.as_ref().is_some_and(StereoWindow::is_open)
    }

    fn close(&mut self) {
        // dropping the window closes it
        self.window = None;
    }
}
