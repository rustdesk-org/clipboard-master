// Derived from https://github.com/Decodetalkers/wayland-clipboard-listener/blob/master/src/dispatch.rs
// Extended to support both ext_data_control_v1 (preferred) and zwlr_data_control_v1 (fallback).

use std::{
    io,
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::Duration,
};
use wayland_client::{
    backend::WaylandError,
    event_created_child,
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, DispatchError, EventQueue, Proxy,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1,
    ext_data_control_source_v1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

const WL_SEAT_NAME_VERSION: u32 = 2;
const INITIALIZATION_RETRY_INTERVAL: Duration = Duration::from_millis(30);
const INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(5);

fn bind_version<I: Proxy>(advertised_version: u32) -> u32 {
    advertised_version.min(I::interface().version)
}

#[derive(Debug)]
pub(crate) struct ClipBoardListenMessage {
    pub _mime_types: Vec<String>,
    pub is_initial: bool,
}

enum DataControlManager {
    Ext(ext_data_control_manager_v1::ExtDataControlManagerV1),
    Zwlr(zwlr_data_control_manager_v1::ZwlrDataControlManagerV1),
}

enum DataControlDevice {
    Ext(ext_data_control_device_v1::ExtDataControlDeviceV1),
    Zwlr(zwlr_data_control_device_v1::ZwlrDataControlDeviceV1),
}

pub(crate) struct WlClipboardListener {
    seat: Option<wl_seat::WlSeat>,
    seat_name: Option<String>,
    seat_name_supported: bool,
    data_manager: Option<DataControlManager>,
    data_device: Option<DataControlDevice>,
    terminated_reason: Option<&'static str>,
    mime_types: Vec<String>,
    queue: Option<Arc<Mutex<EventQueue<Self>>>>,
    exit_flag: Arc<AtomicBool>,
    copied: bool,
    // The first selection event is the existing state, even when its offer is null.
    selection_received: bool,
    // A later selection in the same dispatch batch must be reported as a live change.
    initial_selection: bool,
}

impl WlClipboardListener {
    pub(crate) fn init(exit_flag: Arc<AtomicBool>) -> Result<Self, io::Error> {
        let conn = Connection::connect_to_env().map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "Cannot connect to wayland server, is it running?",
            )
        })?;
        let mut event_queue = conn.new_event_queue();
        let qhandle = event_queue.handle();
        let display = conn.display();

        display.get_registry(&qhandle, ());
        let mut state = WlClipboardListener {
            seat: None,
            seat_name: None,
            seat_name_supported: false,
            data_manager: None,
            data_device: None,
            terminated_reason: None,
            mime_types: Vec::new(),
            queue: None,
            exit_flag,
            copied: false,
            selection_received: false,
            initial_selection: false,
        };
        event_queue.blocking_dispatch(&mut state).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Inital dispatch failed: {e}"))
        })?;
        if !state.device_ready() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Cannot get seat and data manager (neither ext_data_control_v1 nor zwlr_data_control_v1 available)",
            ));
        }
        if state.seat_name_supported {
            while state.seat_name.is_none() {
                event_queue.roundtrip(&mut state).map_err(|_| {
                    io::Error::new(io::ErrorKind::Other, "Cannot roundtrip during init")
                })?;
            }
        }

        state.initialize_data_device(&mut event_queue)?;
        state.queue = Some(Arc::new(Mutex::new(event_queue)));
        Ok(state)
    }

    fn initialize_data_device(&mut self, queue: &mut EventQueue<Self>) -> io::Result<()> {
        self.set_data_device(&queue.handle());
        // Both protocols send an initial selection, including an empty one, after binding.
        self.wait_for_initial_selection(|state| {
            let dispatched = queue.dispatch_pending(state)?;
            if dispatched > 0 {
                return Ok(dispatched);
            }
            match queue.flush() {
                Ok(()) => {}
                // Keep reading when the send buffer is full so the compositor can progress.
                Err(WaylandError::Io(ref error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            // This connection has no other readers during initialization.
            if let Some(guard) = queue.prepare_read() {
                guard.read()?;
            }
            queue.dispatch_pending(state)
        })
    }

    fn wait_for_initial_selection(
        &mut self,
        mut dispatch: impl FnMut(&mut Self) -> Result<usize, DispatchError>,
    ) -> io::Result<()> {
        let started = std::time::Instant::now();
        loop {
            if let Some(reason) = self.terminated_reason.take() {
                return Err(io::Error::new(io::ErrorKind::Other, reason));
            }
            if self.exit_flag.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Data device initialization cancelled",
                ));
            }
            if self.selection_received {
                return Ok(());
            }
            if started.elapsed() >= INITIALIZATION_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timed out waiting for the initial Wayland clipboard selection",
                ));
            }
            match dispatch(self) {
                Ok(0) => {}
                Ok(_) => continue,
                Err(error) => {
                    let kind = match &error {
                        DispatchError::Backend(WaylandError::Io(error)) => error.kind(),
                        _ => io::ErrorKind::Other,
                    };
                    if kind != io::ErrorKind::WouldBlock {
                        return Err(io::Error::new(kind, error));
                    }
                }
            }
            std::thread::sleep(INITIALIZATION_RETRY_INTERVAL);
        }
    }

    fn device_ready(&self) -> bool {
        self.seat.is_some() && self.data_manager.is_some()
    }

    fn set_data_device(&mut self, qh: &wayland_client::QueueHandle<Self>) {
        match (self.seat.as_ref(), self.data_manager.as_ref()) {
            (Some(seat), Some(DataControlManager::Ext(manager))) => {
                let device = manager.get_data_device(seat, qh, ());
                self.data_device = Some(DataControlDevice::Ext(device));
            }
            (Some(seat), Some(DataControlManager::Zwlr(manager))) => {
                let device = manager.get_data_device(seat, qh, ());
                self.data_device = Some(DataControlDevice::Zwlr(device));
            }
            _ => {}
        }
    }

    fn get_message(&mut self) -> Result<ClipBoardListenMessage, io::Error> {
        let Some(queue) = self.queue.clone() else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Event queue not initialized",
            ));
        };
        let mut queue = queue
            .lock()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Cannot lock queue: {e}")))?;
        loop {
            if let Some(reason) = self.terminated_reason.take() {
                return Err(io::Error::new(io::ErrorKind::Other, reason));
            }

            if self.exit_flag.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Exit signal received, exiting",
                ));
            }

            // Initialization may already have dispatched a selection event.
            if self.copied {
                self.copied = false;
                break;
            }

            queue
                .flush()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Flush failed: {e}")))?;
            let read_guard = queue.prepare_read().ok_or(io::Error::new(
                io::ErrorKind::Other,
                format!("Prepare read failed"),
            ))?;
            match read_guard.read() {
                Ok(c) => {
                    if c > 0 {
                        queue.dispatch_pending(self).map_err(|e| {
                            io::Error::new(
                                io::ErrorKind::Other,
                                format!("Dispatch pending failed: {e}"),
                            )
                        })?;
                        if let Some(reason) = self.terminated_reason.take() {
                            return Err(io::Error::new(io::ErrorKind::Other, reason));
                        }
                    } else {
                        // https://docs.rs/wayland-backend/latest/wayland_backend/rs/client/struct.ReadEventsGuard.html#method.read
                        // It's wired that `read()` return `Ok(0)` if `winit` is in `Cargo.tomml`.
                        // https://github.com/rust-windowing/winit/issues/4380
                        std::thread::sleep(Duration::from_millis(30));
                    }
                }
                Err(WaylandError::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(30));
                }
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("Read failed: {e}"),
                    ));
                }
            }
        }
        Ok(ClipBoardListenMessage {
            _mime_types: std::mem::take(&mut self.mime_types),
            is_initial: self.initial_selection,
        })
    }
}

impl Iterator for WlClipboardListener {
    type Item = Result<ClipBoardListenMessage, io::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.get_message())
    }
}

// --- Registry dispatch: prefer ext_data_control_v1, fall back to zwlr_data_control_v1 ---

impl Dispatch<wl_registry::WlRegistry, ()> for WlClipboardListener {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: <wl_registry::WlRegistry as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        qh: &wayland_client::QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == wl_seat::WlSeat::interface().name {
                if state.seat.is_none() {
                    state.seat_name_supported = version >= WL_SEAT_NAME_VERSION;
                    state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(
                        name,
                        bind_version::<wl_seat::WlSeat>(version),
                        qh,
                        (),
                    ));
                }
            } else if interface
                == ext_data_control_manager_v1::ExtDataControlManagerV1::interface().name
            {
                // Prefer ext protocol (standard, supported by Plasma 6.5+, wlroots 0.18+)
                state.data_manager = Some(DataControlManager::Ext(
                    registry.bind::<ext_data_control_manager_v1::ExtDataControlManagerV1, _, _>(
                        name,
                        bind_version::<ext_data_control_manager_v1::ExtDataControlManagerV1>(
                            version,
                        ),
                        qh,
                        (),
                    ),
                ));
            } else if interface
                == zwlr_data_control_manager_v1::ZwlrDataControlManagerV1::interface().name
            {
                // Only use zwlr if ext is not already bound
                if !matches!(state.data_manager, Some(DataControlManager::Ext(_))) {
                    state.data_manager = Some(DataControlManager::Zwlr(
                        registry
                            .bind::<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _, _>(
                                name,
                                bind_version::<
                                    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
                                >(version),
                                qh,
                                (),
                            ),
                    ));
                }
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WlClipboardListener {
    fn event(
        state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        event: <wl_seat::WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            state.seat_name = Some(name);
        }
    }
}

// --- ext_data_control_v1 dispatch implementations ---

impl Dispatch<ext_data_control_manager_v1::ExtDataControlManagerV1, ()> for WlClipboardListener {
    fn event(
        _state: &mut Self,
        _proxy: &ext_data_control_manager_v1::ExtDataControlManagerV1,
        _event: <ext_data_control_manager_v1::ExtDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_data_control_device_v1::ExtDataControlDeviceV1, ()> for WlClipboardListener {
    fn event(
        state: &mut Self,
        _proxy: &ext_data_control_device_v1::ExtDataControlDeviceV1,
        event: <ext_data_control_device_v1::ExtDataControlDeviceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id: _id } => {}
            ext_data_control_device_v1::Event::Finished => {
                if let Some(DataControlDevice::Ext(device)) = state.data_device.take() {
                    eprintln!(
                        "Wayland ext_data_control_v1 device finished; stopping clipboard listener"
                    );
                    device.destroy();
                }
                state.terminated_reason = Some("Wayland ext_data_control_v1 device finished");
            }
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(offer) = id {
                    offer.destroy();
                }
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                let initial = !std::mem::replace(&mut state.selection_received, true);
                state.initial_selection = initial;
                let Some(offer) = id else {
                    return;
                };
                offer.destroy();
                state.copied = true;
            }
            _ => {}
        }
    }
    event_created_child!(WlClipboardListener, ext_data_control_device_v1::ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ext_data_control_offer_v1::ExtDataControlOfferV1, ())
    ]);
}

impl Dispatch<ext_data_control_source_v1::ExtDataControlSourceV1, ()> for WlClipboardListener {
    fn event(
        _state: &mut Self,
        _proxy: &ext_data_control_source_v1::ExtDataControlSourceV1,
        event: <ext_data_control_source_v1::ExtDataControlSourceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send {
                fd: _fd,
                mime_type: _mime_type,
            } => {}
            _ => {}
        }
    }
}

impl Dispatch<ext_data_control_offer_v1::ExtDataControlOfferV1, ()> for WlClipboardListener {
    fn event(
        state: &mut Self,
        _proxy: &ext_data_control_offer_v1::ExtDataControlOfferV1,
        event: <ext_data_control_offer_v1::ExtDataControlOfferV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.mime_types.push(mime_type);
        }
    }
}

// --- zwlr_data_control_v1 dispatch implementations (fallback for older compositors) ---

impl Dispatch<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, ()> for WlClipboardListener {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        _event: <zwlr_data_control_manager_v1::ZwlrDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for WlClipboardListener {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: <zwlr_data_control_device_v1::ZwlrDataControlDeviceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id: _id } => {}
            zwlr_data_control_device_v1::Event::Finished => {
                if let Some(DataControlDevice::Zwlr(device)) = state.data_device.take() {
                    eprintln!(
                        "Wayland zwlr_data_control_v1 device finished; stopping clipboard listener"
                    );
                    device.destroy();
                }
                state.terminated_reason = Some("Wayland zwlr_data_control_v1 device finished");
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(offer) = id {
                    offer.destroy();
                }
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                let initial = !std::mem::replace(&mut state.selection_received, true);
                state.initial_selection = initial;
                let Some(offer) = id else {
                    return;
                };
                offer.destroy();
                state.copied = true;
            }
            _ => {
                println!("unhandled event: {:?}", event);
            }
        }
    }
    event_created_child!(WlClipboardListener, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ())
    ]);
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for WlClipboardListener {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: <zwlr_data_control_source_v1::ZwlrDataControlSourceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send {
                fd: _fd,
                mime_type: _mime_type,
            } => {}
            _ => {
                eprintln!("unhandled event: {event:?}");
            }
        }
    }
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for WlClipboardListener {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        event: <zwlr_data_control_offer_v1::ZwlrDataControlOfferV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.mime_types.push(mime_type);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    const TEST_GRACE_PERIOD: Duration = Duration::from_secs(2);

    fn listener() -> WlClipboardListener {
        WlClipboardListener {
            seat: None,
            seat_name: None,
            seat_name_supported: false,
            data_manager: None,
            data_device: None,
            terminated_reason: None,
            mime_types: Vec::new(),
            queue: None,
            exit_flag: Arc::new(AtomicBool::new(false)),
            copied: false,
            selection_received: false,
            initial_selection: false,
        }
    }

    fn assert_selection_batches<I: Proxy, O: Proxy>(event: fn(Option<O>) -> I::Event)
    where
        WlClipboardListener: Dispatch<I, ()>,
    {
        let (client, _server) = UnixStream::pair().expect("Create a Wayland socket pair");
        let connection = Connection::from_socket(client).expect("Create a Wayland connection");
        let queue = connection.new_event_queue::<WlClipboardListener>();
        let device = I::inert(connection.backend().downgrade());
        for (received, copied, offers, expected) in [
            (false, false, &[true][..], Some(true)),
            (false, false, &[false][..], None),
            (false, false, &[false, true][..], Some(false)),
            (false, false, &[true, true][..], Some(false)),
            (false, false, &[true, false][..], Some(false)),
            (true, true, &[false][..], Some(false)),
            (true, false, &[false][..], None),
        ] {
            let mut state = listener();
            state.copied = copied;
            state.selection_received = received;
            state.initial_selection = received;
            for &has_offer in offers {
                let id = has_offer.then(|| O::inert(connection.backend().downgrade()));
                <WlClipboardListener as Dispatch<I, ()>>::event(
                    &mut state,
                    &device,
                    event(id),
                    &(),
                    &connection,
                    &queue.handle(),
                );
            }
            assert!(state.selection_received);
            assert_eq!(
                state.copied.then_some(state.initial_selection),
                expected,
                "received={received}, copied={copied}, offers={offers:?}",
            );
        }
    }

    #[test]
    fn ext_selection_batches_classify_startup() {
        use ext_data_control_device_v1::{Event, ExtDataControlDeviceV1};
        use ext_data_control_offer_v1::ExtDataControlOfferV1;
        assert_selection_batches::<ExtDataControlDeviceV1, ExtDataControlOfferV1>(|id| {
            Event::Selection { id }
        });
    }

    #[test]
    fn zwlr_selection_batches_classify_startup() {
        use zwlr_data_control_device_v1::{Event, ZwlrDataControlDeviceV1};
        use zwlr_data_control_offer_v1::ZwlrDataControlOfferV1;
        assert_selection_batches::<ZwlrDataControlDeviceV1, ZwlrDataControlOfferV1>(|id| {
            Event::Selection { id }
        });
    }

    fn assert_silent_peer_initialization(cancel: bool) {
        const SEAT_GLOBAL: u32 = 1;
        const MANAGER_GLOBAL: u32 = 2;
        const PROTOCOL_VERSION: u32 = 1;
        let (client, mut server) = UnixStream::pair().expect("Create a Wayland socket pair");
        server
            .set_read_timeout(Some(TEST_GRACE_PERIOD))
            .expect("Bound the test server read");
        let connection = Connection::from_socket(client).expect("Create a Wayland connection");
        let mut queue = connection.new_event_queue::<WlClipboardListener>();
        let handle = queue.handle();
        let registry = connection.display().get_registry(&handle, ());
        let mut state = listener();
        state.seat = Some(registry.bind(SEAT_GLOBAL, PROTOCOL_VERSION, &handle, ()));
        state.data_manager = Some(DataControlManager::Ext(registry.bind(
            MANAGER_GLOBAL,
            PROTOCOL_VERSION,
            &handle,
            (),
        )));
        let exit_flag = state.exit_flag.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender
                .send(state.initialize_data_device(&mut queue))
                .expect("Report the initialization result");
        });
        // Wait for a real request before signalling, while keeping the peer silent and open.
        let request = server.read_exact(&mut [0]);
        let wait = if cancel {
            exit_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            TEST_GRACE_PERIOD
        } else {
            INITIALIZATION_TIMEOUT + TEST_GRACE_PERIOD
        };
        let result = receiver.recv_timeout(wait);
        drop(server); // Release a regressed blocking read before joining the worker.
        worker.join().expect("Join the initialization worker");
        request.expect("Initialization must flush its requests");
        let error = result
            .expect("Initialization must finish with the peer still open")
            .expect_err("A silent peer cannot establish readiness");
        let expected = if cancel {
            io::ErrorKind::Interrupted
        } else {
            io::ErrorKind::TimedOut
        };
        assert_eq!(error.kind(), expected);
    }

    #[test]
    fn initialization_with_silent_peer_can_be_cancelled() {
        assert_silent_peer_initialization(true);
    }

    #[test]
    fn initialization_with_silent_peer_times_out() {
        assert_silent_peer_initialization(false);
    }
}
