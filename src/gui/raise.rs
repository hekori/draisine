//! Bringing the running window to the front when the tray asks for it, or minimizing it when
//! the tray icon is clicked while the window is in front.
//!
//! The request file is watched on a background thread: a minimized window may not get any frames
//! (on Wayland the compositor stops frame callbacks), so polling from `ui()` would never see it.
//!
//! On X11 and Windows the window un-minimizes itself with viewport commands in `ui()`. Wayland
//! has no way to do that; the window has to present the XDG activation token of the user's click
//! (passed along by the tray). winit can only use a token when it creates a window, so for the
//! existing window this talks `xdg_activation_v1` to the compositor directly.

use crate::engine::Mode;
use crate::ipc;
use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};

/// What the UI should do next.
pub enum Request {
    /// Un-minimize and focus (already done on Wayland), then start the action if any.
    Show(Option<Mode>),
    Minimize,
}

/// Shared between the UI and the watcher thread.
#[derive(Default)]
struct State {
    /// The latest request not yet handled by the UI.
    request: Option<Request>,
    focused: bool,
    last_focused: Option<Instant>,
    last_frame: Option<Instant>,
}

impl State {
    /// Whether the window is in front. A minimized window may not get frames on Wayland, so its
    /// focus flag can be stale; it then counts as hidden. The grace period covers a tray host
    /// that takes the focus for the click.
    fn in_front(&self) -> bool {
        let recent = |t: Option<Instant>, ms| t.is_some_and(|t| t.elapsed() < Duration::from_millis(ms));
        recent(self.last_frame, 1500) && (self.focused || recent(self.last_focused, 600))
    }
}

#[derive(Clone)]
pub struct Watcher(Arc<Mutex<State>>);

impl Watcher {
    /// Call every frame: records the focus and takes the pending request.
    pub fn frame(&self, ctx: &egui::Context) -> Option<Request> {
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(false);
        let mut st = self.0.lock().unwrap();
        let now = Instant::now();
        st.last_frame = Some(now);
        if st.focused && !focused {
            st.last_focused = Some(now);
        }
        st.focused = focused;
        st.request.take()
    }
}

/// Starts watching for window requests; `win` is the window's creation context.
pub fn watch(ctx: &egui::Context, win: &(impl HasWindowHandle + HasDisplayHandle)) -> Watcher {
    let watcher = Watcher(Arc::new(Mutex::new(State::default())));
    let activator = Activator::new(win);
    let (shared, ctx) = (watcher.0.clone(), ctx.clone());
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let Some(req) = ipc::take_request() else { continue };
            let mut st = shared.lock().unwrap();
            if req.toggle && st.in_front() {
                st.request = Some(Request::Minimize);
            } else {
                if let Some(a) = &activator {
                    match &req.token {
                        Some(t) => a.activate(t),
                        None => crate::log::warn("Window request without an activation token: cannot raise the window"),
                    }
                }
                st.request = Some(Request::Show(req.action));
            }
            drop(st);
            ctx.request_repaint();
        }
    });
    watcher
}

#[cfg(target_os = "linux")]
use wayland::Activator;

#[cfg(not(target_os = "linux"))]
struct Activator;

#[cfg(not(target_os = "linux"))]
impl Activator {
    fn new(_: &(impl HasWindowHandle + HasDisplayHandle)) -> Option<Activator> {
        None
    }
    fn activate(&self, _: &str) {}
}

#[cfg(target_os = "linux")]
mod wayland {
    use wayland_client::backend::{Backend, ObjectId};
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::wl_registry::{self, WlRegistry};
    use wayland_client::protocol::wl_surface::WlSurface;
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
    use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
    use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};

    pub struct State;

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(_: &mut Self, _: &WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
    delegate_noop!(State: ignore XdgActivationV1);

    /// `xdg_activation_v1` bound on winit's Wayland connection, plus the window's surface.
    pub struct Activator {
        conn: Connection,
        _queue: EventQueue<State>,
        activation: XdgActivationV1,
        surface: WlSurface,
    }

    impl Activator {
        /// None when not running on Wayland, or the compositor lacks xdg-activation.
        pub fn new(win: &(impl HasWindowHandle + HasDisplayHandle)) -> Option<Activator> {
            let (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) =
                (win.display_handle().ok()?.as_raw(), win.window_handle().ok()?.as_raw())
            else {
                return None;
            };
            // SAFETY: winit owns the display and the surface; both live as long as the window,
            // and the window lives as long as this process.
            let conn = Connection::from_backend(unsafe { Backend::from_foreign_display(d.display.as_ptr().cast()) });
            let (globals, queue) = registry_queue_init::<State>(&conn).ok()?;
            let activation = globals.bind::<XdgActivationV1, _, _>(&queue.handle(), 1..=1, ()).ok()?;
            // SAFETY: see above; the pointer is a live wl_surface proxy.
            let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), w.surface.as_ptr().cast()) }.ok()?;
            let surface = WlSurface::from_id(&conn, id).ok()?;
            Some(Activator { conn, _queue: queue, activation, surface })
        }

        /// Asks the compositor to un-minimize and focus the window.
        pub fn activate(&self, token: &str) {
            self.activation.activate(token.to_string(), &self.surface);
            if let Err(e) = self.conn.flush() {
                crate::log::error(format!("Raising the window failed: {e}"));
            }
        }
    }
}
