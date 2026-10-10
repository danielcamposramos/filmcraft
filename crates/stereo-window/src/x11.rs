//! The X11 window: a plain window showing the picture, declared with window properties while it
//! shows a pair.
//!
//! The properties are the ones Sparky Stereo OS's stereo declaration defines for X11 (version 2 of the root
//! window's `_KDE_NET_WM_STEREO_CONTENT_SUPPORTED`): `_KDE_NET_WM_STEREO_CONTENT` holds the
//! packing (3, full side by side, left eye first). From version 3 the compositor doubles the window of a program that
//! declares itself, so the window is created at one eye's size; before that the program makes the
//! window as wide as its two eyes. Either way the picture is scaled to the window as it is.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, ImageOrder, PropMode, Screen, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::{Change, Command, Declaration, Error, Picture, SIDE_BY_SIDE_FULL, Shared, convert_row, fit, plan};

/// Connects, creates the window and starts its thread.
pub(crate) fn open(title: &str, app_id: &str, shared: Arc<Shared>) -> Result<(mpsc::Sender<Command>, JoinHandle<()>), Error> {
    let (tx, rx) = mpsc::channel::<Command>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Error>>();
    let (title, app_id) = (title.to_string(), app_id.to_string());
    let thread = std::thread::Builder::new()
        .name("stereo-window".into())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&title, &app_id, &shared, &rx, &ready_tx)));
            if outcome.is_err() {
                let _ = ready_tx.send(Err(Error::X11("the window thread panicked".into())));
            }
            shared.alive.store(false, Ordering::Release);
            shared.declared.store(false, Ordering::Release);
        })
        .map_err(|e| Error::X11(e.to_string()))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok((tx, thread)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::X11("the window thread ended before it was ready".into()))
        }
    }
}

fn x11<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> Error + '_ {
    move |e| Error::X11(format!("{what}: {e}"))
}

struct Display {
    conn: RustConnection,
    window: Window,
    gc: u32,
    depth: u8,
    msb_first: bool,
    /// The compositor's declaration version (0: none).
    support: u32,
    atoms: Atoms,
}

#[derive(Clone, Copy)]
struct Atoms {
    content: u32,
    supported: u32,
    delete: u32,
    protocols: u32,
    net_wm_name: u32,
    utf8: u32,
}

fn atoms(conn: &RustConnection) -> Result<Atoms, Error> {
    let intern = |name: &[u8]| -> Result<u32, Error> { Ok(conn.intern_atom(false, name).map_err(x11("intern"))?.reply().map_err(x11("intern"))?.atom) };
    Ok(Atoms {
        content: intern(b"_KDE_NET_WM_STEREO_CONTENT")?,
        supported: intern(b"_KDE_NET_WM_STEREO_CONTENT_SUPPORTED")?,
        delete: intern(b"WM_DELETE_WINDOW")?,
        protocols: intern(b"WM_PROTOCOLS")?,
        net_wm_name: intern(b"_NET_WM_NAME")?,
        utf8: intern(b"UTF8_STRING")?,
    })
}

/// The declaration version the root window announces, 0 when it does not.
fn support(conn: &RustConnection, root: Window, atom: u32) -> u32 {
    conn.get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().and_then(|mut v| v.next()))
        .unwrap_or(0)
}

/// A 32-bit-per-pixel format for the screen's depth, or an error.
fn check_format(conn: &RustConnection, screen: &Screen) -> Result<(u8, bool), Error> {
    let setup = conn.setup();
    let ok = setup.pixmap_formats.iter().any(|f| f.depth == screen.root_depth && f.bits_per_pixel == 32);
    if !ok {
        return Err(Error::X11(format!("the screen's depth {} has no 32-bit pixel format", screen.root_depth)));
    }
    Ok((screen.root_depth, setup.image_byte_order == ImageOrder::MSB_FIRST))
}

fn run(title: &str, app_id: &str, shared: &Arc<Shared>, commands: &mpsc::Receiver<Command>, ready: &mpsc::Sender<Result<(), Error>>) {
    let mut display = match setup(title, app_id) {
        Ok(d) => d,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    shared.desktop_declares.store(display.support >= 2, Ordering::Release);
    shared.alive.store(true, Ordering::Release);
    let _ = ready.send(Ok(()));

    let mut picture: Option<Picture> = None;
    let mut declaration = Declaration::default();
    let mut size = (0u32, 0u32);
    let mut mapped = false;
    'outer: loop {
        // events first, then a wait on the commands (also the pace of the loop)
        let mut redraw = false;
        loop {
            match display.conn.poll_for_event() {
                Ok(Some(Event::ConfigureNotify(e))) => {
                    // the synthetic copy carries the program's own size, not the window's
                    if e.response_type & 0x80 == 0 {
                        size = (u32::from(e.width), u32::from(e.height));
                        redraw = true;
                    }
                }
                Ok(Some(Event::Expose(_))) => redraw = true,
                Ok(Some(Event::ClientMessage(m))) if m.data.as_data32()[0] == display.atoms.delete => break 'outer,
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    log::warn!("stereo window: {e}");
                    break 'outer;
                }
            }
        }
        match commands.recv_timeout(Duration::from_millis(15)) {
            Ok(Command::Show(p)) => {
                if !mapped {
                    if let Err(e) = display.map(&p) {
                        log::warn!("stereo window: {e}");
                        break;
                    }
                    mapped = true;
                    size = initial_size(&display, &p);
                }
                picture = Some(p);
                redraw = true;
            }
            Ok(Command::Close) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if redraw
            && mapped
            && let Some(p) = &picture
        {
            let decided = plan(p.is_pair(), display.support >= 2);
            match declaration.update(decided.declare) {
                Some(Change::Declare) => {
                    display.declare(true);
                    shared.declared.store(true, Ordering::Release);
                }
                Some(Change::Remove) => {
                    display.declare(false);
                    shared.declared.store(false, Ordering::Release);
                }
                None => {}
            }
            if let Err(e) = display.draw(p, decided.left_eye_only, size) {
                log::warn!("stereo window: {e}");
                break;
            }
        }
    }
    display.declare(false);
    let _ = display.conn.destroy_window(display.window);
    let _ = display.conn.flush();
}

/// The window size a picture asks for: one eye's size fitted to 1280 x 720, doubled in width for
/// a pair when the compositor leaves the doubling to the program.
fn window_size(p: &Picture, support: u32) -> (u32, u32) {
    let (w, h) = fit(p.eye_width(), p.height, None);
    if p.is_pair() && (2..3).contains(&support) { (w.saturating_mul(2), h) } else { (w, h) }
}

fn initial_size(display: &Display, p: &Picture) -> (u32, u32) {
    window_size(p, display.support)
}

fn setup(title: &str, app_id: &str) -> Result<Display, Error> {
    let (conn, screen_num) = x11rb::connect(None).map_err(x11("connect"))?;
    let screen = conn.setup().roots.get(screen_num).ok_or_else(|| Error::X11("no screen".into()))?.clone();
    let (depth, msb_first) = check_format(&conn, &screen)?;
    let atoms = atoms(&conn)?;
    let support = support(&conn, screen.root, atoms.supported);
    let window = conn.generate_id().map_err(x11("window id"))?;
    let gc = conn.generate_id().map_err(x11("gc id"))?;
    let aux = CreateWindowAux::new().background_pixel(screen.black_pixel).event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY);
    conn.create_window(depth, window, screen.root, 0, 0, 640, 360, 0, WindowClass::INPUT_OUTPUT, screen.root_visual, &aux).map_err(x11("create window"))?;
    conn.create_gc(gc, window, &CreateGCAux::new()).map_err(x11("create gc"))?;
    conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes()).map_err(x11("title"))?;
    conn.change_property8(PropMode::REPLACE, window, atoms.net_wm_name, atoms.utf8, title.as_bytes()).map_err(x11("title"))?;
    let class = format!("{app_id}\0{app_id}\0");
    conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_CLASS, AtomEnum::STRING, class.as_bytes()).map_err(x11("class"))?;
    conn.change_property32(PropMode::REPLACE, window, atoms.protocols, AtomEnum::ATOM, &[atoms.delete]).map_err(x11("protocols"))?;
    conn.flush().map_err(x11("flush"))?;
    Ok(Display { conn, window, gc, depth, msb_first, support, atoms })
}

impl Display {
    /// Sizes the window for the first picture and shows it.
    fn map(&mut self, p: &Picture) -> Result<(), Error> {
        let (w, h) = window_size(p, self.support);
        let aux = x11rb::protocol::xproto::ConfigureWindowAux::new().width(w).height(h);
        self.conn.configure_window(self.window, &aux).map_err(x11("size"))?;
        self.conn.map_window(self.window).map_err(x11("map"))?;
        self.conn.flush().map_err(x11("flush"))
    }

    /// Sets or deletes the declaration properties.
    fn declare(&self, on: bool) {
        let a = self.atoms;
        let sent = if on {
            self.conn.change_property32(PropMode::REPLACE, self.window, a.content, AtomEnum::CARDINAL, &[SIDE_BY_SIDE_FULL]).map(|_| ())
        } else {
            self.conn.delete_property(self.window, a.content).map(|_| ())
        };
        if let Err(e) = sent {
            log::warn!("stereo window: declaration: {e}");
        }
        let _ = self.conn.flush();
    }

    /// Draws the picture scaled to the window: all of it, or its left eye.
    fn draw(&self, p: &Picture, left_eye_only: bool, size: (u32, u32)) -> Result<(), Error> {
        let (dw, dh) = (size.0 as usize, size.1 as usize);
        // an X11 image is at most 65535 wide, and its bands are placed by a signed 16-bit row
        if dw == 0 || dh == 0 || dw > 0xFFFF || dh > 0x7FFF {
            return Ok(());
        }
        let src_w = if left_eye_only { p.eye_width() } else { p.width };
        // a row of the source at the destination's width, nearest neighbour
        let mut rgba_row = vec![0u8; dw * 4];
        let mut out = vec![0u8; dw * 4 * dh];
        let x_map: Vec<usize> = (0..dw).map(|x| (x * src_w / dw).min(src_w - 1)).collect();
        for (y, out_row) in out.chunks_exact_mut(dw * 4).enumerate() {
            let sy = (y * p.height / dh).min(p.height - 1);
            let src_row = p.rgba.get(sy * p.width * 4..(sy + 1) * p.width * 4).ok_or(Error::BadPicture)?;
            for (x, sx) in x_map.iter().enumerate() {
                rgba_row[x * 4..x * 4 + 4].copy_from_slice(&src_row[sx * 4..sx * 4 + 4]);
            }
            convert_row(&rgba_row, out_row, self.msb_first);
        }
        // PutImage in bands that fit the server's request size
        let max = self.conn.maximum_request_bytes().saturating_sub(64).max(4096);
        let band = (max / (dw * 4)).clamp(1, dh);
        for (i, chunk) in out.chunks(band * dw * 4).enumerate() {
            let rows = chunk.len() / (dw * 4);
            self.conn
                .put_image(ImageFormat::Z_PIXMAP, self.window, self.gc, dw as u16, rows as u16, 0, (i * band) as i16, 0, self.depth, chunk)
                .map_err(x11("put image"))?;
        }
        self.conn.flush().map_err(x11("flush"))
    }
}
