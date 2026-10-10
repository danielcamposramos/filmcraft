//! The Wayland window: an xdg toplevel showing a `wl_shm` buffer through the viewport protocol,
//! declared with `kde_stereo_content_v1` while it shows a pair.
//!
//! The buffer is the whole picture (a pair is full side by side). The viewport gives the surface
//! the size of one eye, which is the size KWin places a declared window at; on a desktop without
//! the declaration the viewport crops the buffer to its left eye.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread::JoinHandle;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{self, Channel, Event as ChannelEvent};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState, SimpleGlobal};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_simple, delegate_xdg_shell, delegate_xdg_window};
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::viewporter::client::wp_viewport::{self, WpViewport};
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use crate::{Change, Command, Declaration, Error, Picture, Shared, convert_row, fit, plan};

#[allow(clippy::all, unused_imports, dead_code, missing_docs)]
mod stereo_protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/kde-stereo-content-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/kde-stereo-content-v1.xml");
}
use stereo_protocol::kde_stereo_content_manager_v1::KdeStereoContentManagerV1;
use stereo_protocol::kde_stereo_content_v1::{self, KdeStereoContentV1};

/// Sends commands to the window thread.
pub(crate) struct CommandSender(channel::Sender<Command>);

impl CommandSender {
    pub fn send(&self, c: Command) -> Result<(), ()> {
        self.0.send(c).map_err(|_| ())
    }
}

/// Connects, creates the window and starts its thread. Returns once the thread has the globals it
/// needs (or says why not).
pub(crate) fn open(title: &str, app_id: &str, shared: Arc<Shared>) -> Result<(CommandSender, JoinHandle<()>), Error> {
    let (tx, rx) = channel::channel::<Command>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Error>>();
    let (title, app_id) = (title.to_string(), app_id.to_string());
    let thread = std::thread::Builder::new()
        .name("stereo-window".into())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&title, &app_id, &shared, rx, &ready_tx)));
            if outcome.is_err() {
                let _ = ready_tx.send(Err(Error::Wayland("the window thread panicked".into())));
            }
            shared.alive.store(false, Ordering::Release);
            shared.declared.store(false, Ordering::Release);
        })
        .map_err(|e| Error::Wayland(e.to_string()))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok((CommandSender(tx), thread)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::Wayland("the window thread ended before it was ready".into()))
        }
    }
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    viewporter: SimpleGlobal<WpViewporter, 1>,
    pool: SlotPool,
    window: Window,
    viewport: WpViewport,
    manager: Option<KdeStereoContentManagerV1>,
    content: Option<KdeStereoContentV1>,
    declaration: Declaration,
    picture: Option<Picture>,
    configured: bool,
    size: Option<(u32, u32)>,
    exit: bool,
    shared: Arc<Shared>,
    qh: QueueHandle<State>,
}

fn run(title: &str, app_id: &str, shared: &Arc<Shared>, commands: Channel<Command>, ready: &mpsc::Sender<Result<(), Error>>) {
    match setup(title, app_id, shared) {
        Ok((mut event_loop, mut state)) => {
            let handle: LoopHandle<'static, State> = event_loop.handle();
            let inserted = handle.insert_source(commands, |event, _, state| match event {
                ChannelEvent::Msg(Command::Show(p)) => state.show(p),
                ChannelEvent::Msg(Command::Close) | ChannelEvent::Closed => state.exit = true,
            });
            if let Err(e) = inserted {
                let _ = ready.send(Err(Error::Wayland(format!("event loop: {e}"))));
                return;
            }
            shared.desktop_declares.store(state.manager.is_some(), Ordering::Release);
            shared.alive.store(true, Ordering::Release);
            let _ = ready.send(Ok(()));
            while !state.exit {
                if let Err(e) = event_loop.dispatch(std::time::Duration::from_millis(100), &mut state) {
                    log::warn!("stereo window: {e}");
                    break;
                }
            }
            state.undeclare();
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

fn setup(title: &str, app_id: &str, shared: &Arc<Shared>) -> Result<(EventLoop<'static, State>, State), Error> {
    let wl = |what: &str, e: &dyn std::fmt::Display| Error::Wayland(format!("{what}: {e}"));
    let conn = Connection::connect_to_env().map_err(|e| wl("connect", &e))?;
    let (globals, event_queue): (GlobalList, _) = registry_queue_init(&conn).map_err(|e| wl("registry", &e))?;
    let qh: QueueHandle<State> = event_queue.handle();
    let event_loop: EventLoop<'static, State> = EventLoop::try_new().map_err(|e| wl("event loop", &e))?;
    WaylandSource::new(conn.clone(), event_queue).insert(event_loop.handle()).map_err(|e| wl("event loop", &e))?;

    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| wl("wl_compositor", &e))?;
    let xdg_shell = XdgShell::bind(&globals, &qh).map_err(|e| wl("xdg_wm_base", &e))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| wl("wl_shm", &e))?;
    let viewporter = SimpleGlobal::<WpViewporter, 1>::bind(&globals, &qh).map_err(|e| wl("wp_viewporter", &e))?;
    // present when the desktop announces the stereo declaration
    let manager = globals.bind::<KdeStereoContentManagerV1, _, _>(&qh, 1..=1, ()).ok();

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title(title);
    window.set_app_id(app_id);
    window.set_min_size(Some((160, 90)));
    let viewport = viewporter.get().map_err(|e| wl("wp_viewporter", &e))?.get_viewport(window.wl_surface(), &qh, ());
    // The window is mapped by its first commit; the first picture is drawn at the configure.
    window.commit();
    let pool = SlotPool::new(4096, &shm).map_err(|e| wl("shm pool", &e))?;
    let state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        viewporter,
        pool,
        window,
        viewport,
        manager,
        content: None,
        declaration: Declaration::default(),
        picture: None,
        configured: false,
        size: None,
        exit: false,
        shared: shared.clone(),
        qh,
    };
    Ok((event_loop, state))
}

impl State {
    fn show(&mut self, picture: Picture) {
        self.picture = Some(picture);
        self.draw();
    }

    /// Declares the surface: full side by side, left eye first.
    fn declare(&mut self, qh: &QueueHandle<State>) {
        let Some(manager) = &self.manager else { return };
        if self.content.is_none() {
            self.content = Some(manager.create(self.window.wl_surface(), qh, ()));
        }
        if let Some(content) = &self.content {
            content.set_content(kde_stereo_content_v1::Content::SideBySideFull);
        }
        self.shared.declared.store(true, Ordering::Release);
    }

    /// Removes the declaration; it takes effect with the next commit of the surface.
    fn undeclare(&mut self) {
        if let Some(content) = self.content.take() {
            content.destroy();
        }
        self.shared.declared.store(false, Ordering::Release);
    }

    fn draw(&mut self) {
        if !self.configured {
            return;
        }
        let Some(pic) = self.picture.clone() else { return };
        let qh = self.qh.clone();
        let decided = plan(pic.is_pair(), self.manager.is_some());
        match self.declaration.update(decided.declare) {
            Some(Change::Declare) => self.declare(&qh),
            Some(Change::Remove) => self.undeclare(),
            None => {}
        }
        let (Ok(w), Ok(h)) = (i32::try_from(pic.width), i32::try_from(pic.height)) else { return };
        let Some(stride) = w.checked_mul(4) else { return };
        let (buffer, canvas) = match self.pool.create_buffer(w, h, stride, wl_shm::Format::Xrgb8888) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("stereo window: buffer: {e}");
                return;
            }
        };
        for (src, dst) in pic.rgba.chunks_exact(pic.width * 4).zip(canvas.chunks_exact_mut(pic.width * 4)) {
            convert_row(src, dst, false);
        }
        // What one eye's place shows: the left eye of a pair (the whole buffer is declared, or
        // cropped to its left half), else the picture.
        let eye_w = pic.eye_width();
        let source_w = if decided.left_eye_only { eye_w } else { pic.width };
        let (dw, dh) = fit(eye_w, pic.height, self.size);
        self.viewport.set_source(0.0, 0.0, source_w as f64, pic.height as f64);
        self.viewport.set_destination(i32::try_from(dw).unwrap_or(i32::MAX), i32::try_from(dh).unwrap_or(i32::MAX));
        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_err() {
            return;
        }
        surface.commit();
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window, configure: WindowConfigure, _: u32) {
        self.size = match configure.new_size {
            (Some(w), Some(h)) => Some((w.get(), h.get())),
            _ => None,
        };
        self.configured = true;
        self.draw();
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers!(OutputState);
}

impl AsMut<SimpleGlobal<WpViewporter, 1>> for State {
    fn as_mut(&mut self) -> &mut SimpleGlobal<WpViewporter, 1> {
        &mut self.viewporter
    }
}

// The viewport and the two stereo objects have no events.
impl Dispatch<WpViewport, ()> for State {
    fn event(_: &mut State, _: &WpViewport, _: wp_viewport::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
}

impl Dispatch<KdeStereoContentManagerV1, ()> for State {
    fn event(
        _: &mut State,
        _: &KdeStereoContentManagerV1,
        _: stereo_protocol::kde_stereo_content_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<KdeStereoContentV1, ()> for State {
    fn event(_: &mut State, _: &KdeStereoContentV1, _: kde_stereo_content_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_simple!(State, WpViewporter, 1);
delegate_registry!(State);

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated protocol carries the values the library says it declares.
    #[test]
    fn the_declared_values_are_the_wire_values() {
        assert_eq!(kde_stereo_content_v1::Content::SideBySideFull as u32, crate::SIDE_BY_SIDE_FULL);
    }
}
