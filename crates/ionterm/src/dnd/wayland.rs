//! Drops on Wayland, through our own `wl_data_device` on winit's connection.
//!
//! winit 0.30 never binds a data device, so this one is the only one and
//! gets every drag over any of the app's windows; each event names the
//! surface it's over (`dnd::surface_key`), which the app maps to a window. It lives on its own event queue and
//! thread: winit skips its loop iteration when socket data was only meant
//! for another queue, so dispatching from winit's thread could leave a drop
//! waiting. All Wayland calls stay on that thread (reading the dropped data
//! too), so stopping it before winit disconnects is enough to be safe.

use std::io::{ErrorKind, PipeReader, Read};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::thread::JoinHandle;

use rustix::event::{PollFd, PollFlags, poll};
use wayland_backend::client::Backend;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, event_created_child};
use winit::event_loop::EventLoopProxy;
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use winit::window::Window;

use super::{DragEvent, parse_uri_list};
use crate::app::UserEvent;

const URI_LIST: &str = "text/uri-list";
/// More than any real list of dropped files; stops a runaway source.
const MAX_LIST_BYTES: usize = 4 << 20;

/// The listening thread. Dropping it stops and joins the thread, which must
/// happen before winit disconnects from the display.
pub struct Listener {
    stop: Option<UnixStream>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Closing our end wakes the thread's poll.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts listening when the window is on Wayland.
pub fn listen(window: &Window, proxy: EventLoopProxy<UserEvent>) -> Option<Listener> {
    let RawDisplayHandle::Wayland(handle) = window.display_handle().ok()?.as_raw() else { return None };
    // SAFETY: the display is winit's live connection, and `App::exiting`
    // drops the listener (joining its thread) before winit disconnects it.
    let backend = unsafe { Backend::from_foreign_display(handle.display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    match start(conn, proxy) {
        Ok(listener) => {
            tracing::debug!("listening for file drops");
            Some(listener)
        }
        Err(error) => {
            tracing::warn!("file drops won't work: {error}");
            None
        }
    }
}

fn start(conn: Connection, proxy: EventLoopProxy<UserEvent>) -> anyhow::Result<Listener> {
    let (globals, queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    // Version 3 adds drag actions; versions 1 and 2 drop without them.
    let manager: WlDataDeviceManager = globals.bind(&qh, 1..=3, ())?;
    let seat: WlSeat = globals.bind(&qh, 1..=1, ())?;
    let device = manager.get_data_device(&seat, &qh, ());
    let (stop, wake) = UnixStream::pair()?;
    let state = State {
        proxy,
        _device: device,
        offer: None,
        accepted: false,
        position: (0.0, 0.0),
        surface: 0,
        reading: None,
    };
    let thread = std::thread::Builder::new()
        .name("wayland-drops".into())
        .spawn(move || run(queue, state, wake))?;
    Ok(Listener {
        stop: Some(stop),
        thread: Some(thread),
    })
}

/// Dispatches drag events until the listener is dropped. This is
/// `blocking_dispatch` with two more things to wait on: the stop socket,
/// and the pipe of a drop being read.
fn run(mut queue: EventQueue<State>, mut state: State, wake: UnixStream) {
    loop {
        if let Err(error) = queue.dispatch_pending(&mut state) {
            tracing::warn!("file drops stopped: {error}");
            return;
        }
        if let Err(error) = queue.flush()
            && !matches!(&error, wayland_backend::client::WaylandError::Io(e) if e.kind() == ErrorKind::WouldBlock)
        {
            tracing::warn!("file drops stopped: {error}");
            return;
        }
        // `None` means events arrived for this queue meanwhile.
        let Some(guard) = queue.prepare_read() else { continue };
        let (wayland_ready, stopped, pipe_ready) = {
            let display = guard.connection_fd();
            let mut fds = vec![
                PollFd::new(&display, PollFlags::IN | PollFlags::ERR),
                PollFd::new(&wake, PollFlags::IN),
            ];
            if let Some(reading) = &state.reading {
                fds.push(PollFd::new(&reading.pipe, PollFlags::IN));
            }
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => {
                    tracing::warn!("file drops stopped: {error}");
                    return;
                }
            }
            let ready = |fd: &PollFd| !fd.revents().is_empty();
            (ready(&fds[0]), ready(&fds[1]), fds.get(2).is_some_and(ready))
        };
        if stopped {
            // Dropping the guard cancels the read.
            return;
        }
        if wayland_ready {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_backend::client::WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
                Err(error) => {
                    tracing::warn!("file drops stopped: {error}");
                    return;
                }
            }
        } else {
            drop(guard);
        }
        if pipe_ready {
            state.read_drop();
        }
    }
}

/// A dropped file list being read from its source.
struct Reading {
    pipe: PipeReader,
    offer: WlDataOffer,
    data: Vec<u8>,
    position: (f64, f64),
    surface: usize,
}

struct State {
    proxy: EventLoopProxy<UserEvent>,
    /// Kept for its lifetime: drags arrive through it.
    _device: WlDataDevice,
    /// The offer of the drag over the window.
    offer: Option<WlDataOffer>,
    /// Whether that drag carries files.
    accepted: bool,
    position: (f64, f64),
    /// The surface the drag is over, as `dnd::surface_key` names it.
    surface: usize,
    reading: Option<Reading>,
}

impl State {
    fn send(&self, event: DragEvent) {
        self.send_to(self.surface, event);
    }

    fn send_to(&self, surface: usize, event: DragEvent) {
        let _ = self.proxy.send_event(UserEvent::Drag(Some(surface), event));
    }

    /// Starts reading the dropped file list; `read_drop` continues it.
    fn take_drop(&mut self) {
        let Some(offer) = self.offer.take() else { return };
        if !self.accepted {
            offer.destroy();
            return;
        }
        // A drop still being read is abandoned for the new one.
        if let Some(old) = self.reading.take() {
            old.offer.destroy();
        }
        let (pipe, writer) = match std::io::pipe() {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!("can't read the dropped files: {error}");
                offer.destroy();
                return;
            }
        };
        // The request sends a copy of the write end, so closing ours lets
        // the read end see the end of the list once the source closes too.
        offer.receive(URI_LIST.into(), std::os::fd::AsFd::as_fd(&writer));
        drop(writer);
        self.reading = Some(Reading {
            pipe,
            offer,
            data: Vec::new(),
            position: self.position,
            surface: self.surface,
        });
    }

    fn read_drop(&mut self) {
        let Some(reading) = &mut self.reading else { return };
        let mut chunk = [0u8; 16 * 1024];
        let done = match reading.pipe.read(&mut chunk) {
            Ok(0) => true,
            Ok(n) => {
                reading.data.extend_from_slice(&chunk[..n]);
                reading.data.len() > MAX_LIST_BYTES
            }
            Err(e) if e.kind() == ErrorKind::Interrupted || e.kind() == ErrorKind::WouldBlock => false,
            Err(error) => {
                tracing::warn!("can't read the dropped files: {error}");
                true
            }
        };
        if !done {
            return;
        }
        let Some(reading) = self.reading.take() else { return };
        // `finish` is only allowed (version 3+) once an action was chosen.
        let action = reading.offer.data::<OfferData>().map(|d| *lock(&d.action));
        if reading.offer.version() >= 3 && action.is_some_and(|a| !a.is_empty()) {
            reading.offer.finish();
        }
        reading.offer.destroy();
        let paths = parse_uri_list(&String::from_utf8_lossy(&reading.data));
        if !paths.is_empty() {
            self.send_to(
                reading.surface,
                DragEvent::Dropped {
                    position: Some(reading.position),
                    paths,
                },
            );
        }
    }
}

/// What a data offer said about itself.
struct OfferData {
    mime_types: Mutex<Vec<String>>,
    /// The action the compositor picked (version 3).
    action: Mutex<DndAction>,
}

impl Default for OfferData {
    fn default() -> Self {
        Self {
            mime_types: Mutex::default(),
            action: Mutex::new(DndAction::empty()),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Dispatch<WlDataDevice, ()> for State {
    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, OfferData::default())
    ]);

    fn event(state: &mut Self, _: &WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_device::Event::Enter { serial, surface, x, y, id } => {
                if let Some(old) = state.offer.take() {
                    old.destroy();
                }
                state.position = (x, y);
                state.surface = surface.id().as_ptr() as usize;
                state.accepted = false;
                let Some(offer) = id else { return };
                let files = offer
                    .data::<OfferData>()
                    .is_some_and(|d| lock(&d.mime_types).iter().any(|m| m == URI_LIST));
                if files {
                    offer.accept(serial, Some(URI_LIST.into()));
                    if offer.version() >= 3 {
                        // Files are copied, never moved, whatever the source offers.
                        offer.set_actions(DndAction::Copy, DndAction::Copy);
                    }
                    state.accepted = true;
                    state.send(DragEvent::Hover(Some((x, y))));
                } else {
                    offer.accept(serial, None);
                }
                state.offer = Some(offer);
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                state.position = (x, y);
                if state.accepted {
                    state.send(DragEvent::Hover(Some((x, y))));
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(offer) = state.offer.take() {
                    offer.destroy();
                }
                if std::mem::take(&mut state.accepted) {
                    state.send(DragEvent::Left);
                }
            }
            wl_data_device::Event::Drop => {
                state.take_drop();
                // A leave may follow, but the drag is over either way.
                if std::mem::take(&mut state.accepted) {
                    state.send(DragEvent::Left);
                }
            }
            // The clipboard is arboard's business; just free the offer.
            wl_data_device::Event::Selection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, OfferData> for State {
    fn event(_: &mut Self, _: &WlDataOffer, event: wl_data_offer::Event, data: &OfferData, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => lock(&data.mime_types).push(mime_type),
            wl_data_offer::Event::Action { dnd_action: WEnum::Value(action) } => *lock(&data.action) = action,
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: wayland_client::protocol::wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: wayland_client::protocol::wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(_: &mut Self, _: &WlDataDeviceManager, _: wayland_client::protocol::wl_data_device_manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
