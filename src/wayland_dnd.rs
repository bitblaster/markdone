//! Files dropped on the window from a file manager, on Wayland.
//!
//! winit 0.30 implements file drag-and-drop only on X11, Windows and macOS. On Wayland
//! this module listens to the seat's `wl_data_device` on a second connection to
//! winit's display, with its own event queue dispatched on a background thread: a drag
//! carrying `text/uri-list` is accepted (copy) and, on drop, its `file://` URIs are
//! decoded and handed to `on_drop`.
//!
//! **Shutdown.** The display belongs to winit, which disconnects it (freeing it) when
//! its event loop ends. The listener must be stopped before that ([`Listener::stop`],
//! from the app's `exiting`): a thread still blocked on the display would read freed
//! memory (seen as "Io error: Cannot allocate memory" and a SIGSEGV at exit). So the
//! thread never blocks indefinitely: it polls the display with a short timeout and
//! checks a stop flag in between.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};

const URI_LIST: &str = "text/uri-list";

type OnDrop = Box<dyn Fn(Vec<PathBuf>) + Send>;

struct State {
    conn: Connection,
    /// MIME types announced by each live offer.
    offers: HashMap<ObjectId, Vec<String>>,
    /// The offer of the drag over the window, and whether it carries files.
    current: Option<(WlDataOffer, bool)>,
    on_drop: OnDrop,
}

/// How long the listener waits on the display before checking its stop flag.
const POLL_MS: i32 = 100;

/// The running listener. Stop it (or drop it) before the display is disconnected.
pub struct Listener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    /// Stop the listener thread and wait for it (at most one poll interval).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start listening for dropped files on the Wayland display behind `display`
/// (a `wl_display*`, e.g. from winit's raw display handle).
///
/// # Safety
/// `display` must be a valid `wl_display`, and stay connected until the returned
/// [`Listener`] is stopped or dropped.
pub unsafe fn start(
    display: *mut std::ffi::c_void,
    on_drop: impl Fn(Vec<PathBuf>) + Send + 'static,
) -> Result<Listener, String> {
    // SAFETY: the caller guarantees `display` is a live wl_display.
    let backend = unsafe { Backend::from_foreign_display(display.cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let manager: WlDataDeviceManager = globals
        .bind(&qh, 1..=3, ())
        .map_err(|e| format!("no data device manager: {e}"))?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=7, ())
        .map_err(|e| format!("no seat: {e}"))?;
    let _device = manager.get_data_device(&seat, &qh, ());
    let mut state = State {
        conn: conn.clone(),
        offers: HashMap::new(),
        current: None,
        on_drop: Box::new(on_drop),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("markdone-dnd".into())
        .spawn(move || {
            // The protocol objects live (and are released) with the thread.
            let keep = (manager, seat, _device);
            while !stop_flag.load(Ordering::Acquire) {
                if queue.dispatch_pending(&mut state).is_err() {
                    break;
                }
                let _ = conn.flush();
                let Some(guard) = queue.prepare_read() else {
                    continue; // events arrived meanwhile: dispatch them first
                };
                let mut fd = libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd for the duration of the call.
                let ready = unsafe { libc::poll(&mut fd, 1, POLL_MS) };
                if ready > 0 {
                    if guard.read().is_err() {
                        break;
                    }
                } else {
                    drop(guard); // timeout (or EINTR): cancel the read, check the flag
                }
            }
            if let Some((offer, _)) = state.current.take() {
                offer.destroy();
            }
            // The manager and the seat have no destructor request to send.
            let (_manager, _seat, device) = keep;
            if device.version() >= 2 {
                device.release();
            }
            let _ = state.conn.flush();
        })
        .map_err(|e| e.to_string())?;
    Ok(Listener {
        stop,
        thread: Some(thread),
    })
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlDataDeviceManager,
        _: <WlDataDeviceManager as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            wl_data_device::Event::Enter { serial, id, .. } => {
                state.release_current();
                let Some(offer) = id else { return };
                let has_files = state
                    .offers
                    .get(&offer.id())
                    .is_some_and(|m| m.iter().any(|m| m == URI_LIST));
                if has_files {
                    offer.accept(serial, Some(URI_LIST.into()));
                    if offer.version() >= 3 {
                        offer.set_actions(DndAction::Copy, DndAction::Copy);
                    }
                } else {
                    offer.accept(serial, None);
                }
                state.current = Some((offer, has_files));
            }
            wl_data_device::Event::Leave => state.release_current(),
            wl_data_device::Event::Drop => {
                if let Some((offer, true)) = state.current.take() {
                    let paths = state.receive_files(&offer);
                    if offer.version() >= 3 {
                        offer.finish();
                    }
                    state.forget(offer);
                    if !paths.is_empty() {
                        (state.on_drop)(paths);
                    }
                } else {
                    state.release_current();
                }
            }
            // The clipboard selection: not ours to use (arboard reads the clipboard).
            wl_data_device::Event::Selection { id: Some(offer) } => state.forget(offer),
            _ => {}
        }
    }

    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl State {
    fn release_current(&mut self) {
        if let Some((offer, _)) = self.current.take() {
            self.forget(offer);
        }
    }

    fn forget(&mut self, offer: WlDataOffer) {
        self.offers.remove(&offer.id());
        offer.destroy();
    }

    /// Read the dropped `text/uri-list` through a pipe.
    fn receive_files(&self, offer: &WlDataOffer) -> Vec<PathBuf> {
        let Ok((mut reader, writer)) = std::io::pipe() else {
            return Vec::new();
        };
        offer.receive(URI_LIST.into(), writer.as_fd());
        let _ = self.conn.flush();
        drop(writer); // the source holds its own copy; EOF comes when it is done
        let mut list = String::new();
        if reader.read_to_string(&mut list).is_err() {
            return Vec::new();
        }
        parse_uri_list(&list)
    }
}

/// Local paths of a `text/uri-list` (RFC 2483): one URI per line, `#` comments; only
/// `file://` URIs on this host count.
pub fn parse_uri_list(list: &str) -> Vec<PathBuf> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|uri| {
            let rest = uri.strip_prefix("file://")?;
            // `file:///path` or `file://localhost/path`; another host is not local.
            let path = if rest.starts_with('/') {
                rest
            } else {
                rest.strip_prefix("localhost")?
            };
            Some(PathBuf::from(crate::links::percent_decode(path)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_decode_to_local_paths() {
        let list = "# comment\r\nfile:///home/u/Documenti/perch%C3%A9%20s%C3%AC.md\r\n\
                    file://localhost/tmp/a.md\r\nhttps://example.com/x.md\r\n\
                    file://server/share/b.md\r\n";
        assert_eq!(
            parse_uri_list(list),
            vec![
                PathBuf::from("/home/u/Documenti/perché sì.md"),
                PathBuf::from("/tmp/a.md"),
            ]
        );
    }

    /// Attaches to a real Wayland session (ignored by default: needs one): the globals
    /// bind on a foreign display, and the listener stops promptly when asked, before the
    /// display is disconnected (as the app does in `exiting`).
    #[test]
    #[ignore]
    fn starts_and_stops_on_a_live_wayland_display() {
        let conn = Connection::connect_to_env().expect("a Wayland session");
        let display = conn.backend().display_ptr();
        let mut listener = unsafe { start(display.cast(), |_| {}) }.expect("listener started");
        std::thread::sleep(std::time::Duration::from_millis(150));
        let t = std::time::Instant::now();
        listener.stop();
        assert!(
            t.elapsed() < std::time::Duration::from_millis(500),
            "stops within a poll"
        );
        assert!(listener.thread.is_none());
        drop(conn); // disconnect only now, like winit after `exiting`
    }
}
