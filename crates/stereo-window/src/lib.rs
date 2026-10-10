//! The Stereo Monitor window: a native window that shows a stereo pair and tells the desktop it
//! is one.
//!
//! The pair is handed over in one format: full side by side, left eye first, both eyes at full
//! size (the left eye in the left half of the picture). The window declares that to the desktop
//! once, for as long as it shows a stereo picture:
//!
//! - **Wayland:** `kde_stereo_content_v1.set_content(side_by_side_full)` (value 3) on the
//!   window's surface, and the viewport protocol gives the surface the size of one eye.
//! - **X11:** the window property `_KDE_NET_WM_STEREO_CONTENT` (value 3), when the root window
//!   announces `_KDE_NET_WM_STEREO_CONTENT_SUPPORTED` version 2 or later.
//!
//! The desktop decides what each screen shows (a 3D mode, a virtual 3D mode, or the left eye on
//! a plain screen); the window has no output modes of its own. A desktop that does not announce
//! the declaration gets the left eye only, so it never shows two squeezed pictures. A picture
//! that is not stereo is shown as it is and never declared.
//!
//! The window runs on its own thread (and its own display connection); [`StereoWindow::show`]
//! hands it the next picture. Nothing here is `unsafe`.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

#[cfg(all(unix, not(target_os = "macos")))]
mod wayland;
#[cfg(all(unix, not(target_os = "macos")))]
mod x11;

/// The value of `kde_stereo_content_v1.content.side_by_side_full` and of the X11 property: both
/// eyes at full size, side by side, left eye first.
pub const SIDE_BY_SIDE_FULL: u32 = 3;

/// Why a window could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Neither `WAYLAND_DISPLAY` nor `DISPLAY` is set.
    #[error("no display: WAYLAND_DISPLAY and DISPLAY are not set")]
    NoDisplay,
    /// The Wayland connection or a required global is missing.
    #[error("Wayland: {0}")]
    Wayland(String),
    /// The X11 connection or window failed.
    #[error("X11: {0}")]
    X11(String),
    /// This platform has no stereo window.
    #[error("the stereo window is not available on this platform")]
    Unsupported,
    /// The picture's buffer is not `width * height * 4` bytes, or one side is zero.
    #[error("the picture is malformed")]
    BadPicture,
}

/// One picture for the window: RGBA8 (straight alpha, composited over black when shown). A stereo
/// picture is full side by side, left eye first, so each eye is `width / 2` wide.
#[derive(Clone)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub rgba: Arc<[u8]>,
    /// The picture holds two views.
    pub stereo: bool,
}

impl Picture {
    /// Checks the buffer against the size.
    pub fn new(width: usize, height: usize, rgba: Arc<[u8]>, stereo: bool) -> Result<Self, Error> {
        let len = width.checked_mul(height).and_then(|n| n.checked_mul(4));
        if width == 0 || height == 0 || len != Some(rgba.len()) {
            return Err(Error::BadPicture);
        }
        Ok(Self { width, height, rgba, stereo })
    }

    /// The width of what the window shows at one eye's place: half of a stereo picture.
    pub(crate) fn eye_width(&self) -> usize {
        if self.stereo && self.width >= 2 { self.width / 2 } else { self.width }
    }

    /// Whether the picture is treated as a pair (a one pixel wide picture cannot hold two eyes).
    pub(crate) fn is_pair(&self) -> bool {
        self.stereo && self.width >= 2
    }
}

/// Which windowing system the window opened on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Wayland,
    X11,
}

/// What the window does with a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Declare full side by side on the window.
    pub declare: bool,
    /// Show the left half only: a pair on a desktop that cannot be told it is one.
    pub left_eye_only: bool,
}

/// A pair is declared when the desktop announces support and shown as its left eye when it does
/// not; a picture that is not a pair is never declared.
pub(crate) fn plan(is_pair: bool, desktop_declares: bool) -> Plan {
    Plan { declare: is_pair && desktop_declares, left_eye_only: is_pair && !desktop_declares }
}

/// A change of the window's declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Declare,
    Remove,
}

/// Remembers whether the window is declared, so a declaration is made once and removed once
/// however many pictures follow.
#[derive(Default, Debug)]
pub(crate) struct Declaration {
    declared: bool,
}

impl Declaration {
    pub fn update(&mut self, want: bool) -> Option<Change> {
        match (self.declared, want) {
            (false, true) => {
                self.declared = true;
                Some(Change::Declare)
            }
            (true, false) => {
                self.declared = false;
                Some(Change::Remove)
            }
            _ => None,
        }
    }
}

/// The size to give a picture of `w` x `h` (one eye's size for a pair) inside `bound`, keeping
/// its shape. Without a bound (the window has no size yet) a large picture is scaled down to fit
/// 1280 x 720 and a small one keeps its size; with one it fills the bound.
pub(crate) fn fit(w: usize, h: usize, bound: Option<(u32, u32)>) -> (u32, u32) {
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let scale = match bound {
        Some((bw, bh)) => (f64::from(bw.max(1)) / w).min(f64::from(bh.max(1)) / h),
        None => (1280.0 / w).min(720.0 / h).min(1.0),
    };
    (((w * scale).round() as u32).max(1), ((h * scale).round() as u32).max(1))
}

/// Straight RGBA8 over black into 32-bit pixels with the bytes in B, G, R, X order (the
/// little-endian `XRGB8888` of Wayland and the usual 24-bit X11 visual). With `msb_first` the
/// bytes are in X, R, G, B order instead (an X server with the other byte order).
pub(crate) fn convert_row(src: &[u8], dst: &mut [u8], msb_first: bool) {
    let (src, _) = src.as_chunks::<4>();
    let (dst, _) = dst.as_chunks_mut::<4>();
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        let a = u32::from(s[3]);
        let over = |c: u8| if a == 255 { c } else { ((u32::from(c) * a + 127) / 255) as u8 };
        let (r, g, b) = (over(s[0]), over(s[1]), over(s[2]));
        *d = if msb_first { [0xFF, r, g, b] } else { [b, g, r, 0xFF] };
    }
}

enum Sender {
    #[cfg(all(unix, not(target_os = "macos")))]
    Wayland(wayland::CommandSender),
    #[cfg(all(unix, not(target_os = "macos")))]
    X11(std::sync::mpsc::Sender<Command>),
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    None,
}

pub(crate) enum Command {
    Show(Picture),
    Close,
}

/// State the window thread shares with its owner.
#[derive(Default)]
pub(crate) struct Shared {
    /// The window exists (cleared when the user closes it or the connection ends).
    pub alive: AtomicBool,
    /// The desktop announced the stereo declaration.
    pub desktop_declares: AtomicBool,
    /// The window is declared right now.
    pub declared: AtomicBool,
}

/// The Stereo Monitor window. Closed when dropped.
pub struct StereoWindow {
    sender: Sender,
    shared: Arc<Shared>,
    backend: Backend,
    thread: Option<JoinHandle<()>>,
}

impl StereoWindow {
    /// Opens the window on the running desktop: Wayland when `WAYLAND_DISPLAY` is set, else X11
    /// when `DISPLAY` is. The window appears with the first picture shown.
    pub fn open(title: &str, app_id: &str) -> Result<Self, Error> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let shared = Arc::new(Shared::default());
            let has = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
            let (sender, backend, thread) = if has("WAYLAND_DISPLAY") {
                let (s, t) = wayland::open(title, app_id, shared.clone())?;
                (Sender::Wayland(s), Backend::Wayland, t)
            } else if has("DISPLAY") {
                let (s, t) = x11::open(title, app_id, shared.clone())?;
                (Sender::X11(s), Backend::X11, t)
            } else {
                return Err(Error::NoDisplay);
            };
            Ok(Self { sender, shared, backend, thread: Some(thread) })
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            let _ = (title, app_id);
            Err(Error::Unsupported)
        }
    }

    /// Which windowing system the window is on.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Shows the next picture. False when the window is gone (closed by the user).
    pub fn show(&self, picture: Picture) -> bool {
        if !self.is_open() {
            return false;
        }
        match &self.sender {
            #[cfg(all(unix, not(target_os = "macos")))]
            Sender::Wayland(s) => s.send(Command::Show(picture)).is_ok(),
            #[cfg(all(unix, not(target_os = "macos")))]
            Sender::X11(s) => s.send(Command::Show(picture)).is_ok(),
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            Sender::None => {
                let _ = picture;
                false
            }
        }
    }

    /// The window is still on screen (the user did not close it).
    pub fn is_open(&self) -> bool {
        self.shared.alive.load(Ordering::Acquire)
    }

    /// The desktop announced the stereo declaration, so a stereo picture is declared; without it
    /// the window shows the left eye only.
    pub fn desktop_declares(&self) -> bool {
        self.shared.desktop_declares.load(Ordering::Acquire)
    }

    /// The window is declared as stereo content right now.
    pub fn is_declared(&self) -> bool {
        self.shared.declared.load(Ordering::Acquire)
    }

    fn stop(&mut self) {
        match &self.sender {
            #[cfg(all(unix, not(target_os = "macos")))]
            Sender::Wayland(s) => {
                let _ = s.send(Command::Close);
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Sender::X11(s) => {
                let _ = s.send(Command::Close);
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            Sender::None => {}
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for StereoWindow {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pair_is_declared_when_the_desktop_can_be_told() {
        assert_eq!(plan(true, true), Plan { declare: true, left_eye_only: false });
    }

    #[test]
    fn a_pair_on_a_desktop_without_the_declaration_shows_the_left_eye() {
        assert_eq!(plan(true, false), Plan { declare: false, left_eye_only: true });
    }

    #[test]
    fn a_picture_that_is_not_a_pair_is_never_declared() {
        assert_eq!(plan(false, true), Plan { declare: false, left_eye_only: false });
        assert_eq!(plan(false, false), Plan { declare: false, left_eye_only: false });
    }

    #[test]
    fn the_declaration_is_made_once_and_removed_once() {
        let mut d = Declaration::default();
        // 2D, then three pictures of a pair, then 2D again
        let wants = [false, true, true, true, false, false];
        let changes: Vec<_> = wants.iter().map(|w| d.update(*w)).collect();
        assert_eq!(changes, vec![None, Some(Change::Declare), None, None, Some(Change::Remove), None]);
    }

    #[test]
    fn nothing_is_declared_while_nothing_stereo_is_shown() {
        let mut d = Declaration::default();
        for _ in 0..10 {
            assert_eq!(d.update(plan(false, true).declare), None);
        }
    }

    #[test]
    fn the_picture_is_checked() {
        let px: Arc<[u8]> = vec![0u8; 2 * 2 * 4].into();
        assert!(Picture::new(2, 2, px.clone(), true).is_ok());
        assert!(matches!(Picture::new(3, 2, px.clone(), false), Err(Error::BadPicture)));
        assert!(matches!(Picture::new(0, 2, px.clone(), false), Err(Error::BadPicture)));
        assert!(matches!(Picture::new(usize::MAX, 2, px, false), Err(Error::BadPicture)));
    }

    #[test]
    fn an_eye_is_half_of_a_pair() {
        let p = Picture::new(4, 1, vec![0u8; 16].into(), true).unwrap();
        assert_eq!((p.eye_width(), p.is_pair()), (2, true));
        let p = Picture::new(4, 1, vec![0u8; 16].into(), false).unwrap();
        assert_eq!((p.eye_width(), p.is_pair()), (4, false));
        // one pixel cannot hold two eyes
        let p = Picture::new(1, 1, vec![0u8; 4].into(), true).unwrap();
        assert_eq!((p.eye_width(), p.is_pair()), (1, false));
    }

    #[test]
    fn fit_keeps_the_shape() {
        // a large eye comes down to the default size, a small one keeps its own
        assert_eq!(fit(1920, 1080, None), (1280, 720));
        assert_eq!(fit(640, 360, None), (640, 360));
        // a bound is filled, one side limiting
        assert_eq!(fit(1920, 1080, Some((960, 900))), (960, 540));
        assert_eq!(fit(1920, 1080, Some((2000, 540))), (960, 540));
        // degenerate sizes never give zero
        assert_eq!(fit(0, 0, Some((0, 0))), (1, 1));
    }

    #[test]
    fn convert_puts_blue_first_and_composites_over_black() {
        let mut out = [0u8; 8];
        convert_row(&[10, 20, 30, 255, 200, 100, 50, 0], &mut out, false);
        assert_eq!(out, [30, 20, 10, 255, 0, 0, 0, 255]);
        convert_row(&[10, 20, 30, 255, 200, 100, 50, 128], &mut out, true);
        assert_eq!(out, [255, 10, 20, 30, 255, 100, 50, 25]);
    }
}
