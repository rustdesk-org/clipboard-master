// Derived from https://github.com/Decodetalkers/wayland-clipboard-listener/blob/master/src/dispatch.rs
// Extended to support both ext_data_control_v1 (preferred) and zwlr_data_control_v1 (fallback).

use std::{
    io,
    sync::{atomic::AtomicBool, Arc, Mutex},
};
use wayland_client::{
    backend::WaylandError,
    event_created_child,
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, Proxy,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1,
    ext_data_control_source_v1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

#[derive(Debug)]
pub(crate) struct ClipBoardListenMessage {
    pub _mime_types: Vec<String>,
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
    data_manager: Option<DataControlManager>,
    data_device: Option<DataControlDevice>,
    mime_types: Vec<String>,
    queue: Option<Arc<Mutex<EventQueue<Self>>>>,
    exit_flag: Arc<AtomicBool>,
    copied: bool,
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
            data_manager: None,
            data_device: None,
            mime_types: Vec::new(),
            queue: None,
            exit_flag,
            copied: false,
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
        while state.seat_name.is_none() {
            event_queue.roundtrip(&mut state).map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "Cannot roundtrip during init")
            })?;
        }

        state.set_data_device(&qhandle);
        state.queue = Some(Arc::new(Mutex::new(event_queue)));
        Ok(state)
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
            if self.exit_flag.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Exit signal received, exiting",
                ));
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
                        if self.copied {
                            self.copied = false;
                            break;
                        }
                    } else {
                        // https://docs.rs/wayland-backend/latest/wayland_backend/rs/client/struct.ReadEventsGuard.html#method.read
                        // It's wired that `read()` return `Ok(0)` if `winit` is in `Cargo.tomml`.
                        // https://github.com/rust-windowing/winit/issues/4380
                        std::thread::sleep(std::time::Duration::from_millis(30));
                    }
                }
                Err(WaylandError::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(30));
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
            _mime_types: self.mime_types.clone(),
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
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version, qh, ()));
            } else if interface
                == ext_data_control_manager_v1::ExtDataControlManagerV1::interface().name
            {
                // Prefer ext protocol (standard, supported by Plasma 6.5+, wlroots 0.18+)
                state.data_manager = Some(DataControlManager::Ext(
                    registry.bind::<ext_data_control_manager_v1::ExtDataControlManagerV1, _, _>(
                        name, version, qh, (),
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
                                name, version, qh, (),
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
        qh: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id: _id } => {}
            ext_data_control_device_v1::Event::Finished => {
                if let Some(DataControlManager::Ext(dm)) = state.data_manager.as_ref() {
                    let source = dm.create_data_source(qh, ());
                    if let Some(DataControlDevice::Ext(dd)) = state.data_device.as_ref() {
                        dd.set_selection(Some(&source));
                    }
                }
            }
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(offer) = id {
                    offer.destroy();
                }
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                let Some(_offer) = id else {
                    return;
                };
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
        qh: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id: _id } => {}
            zwlr_data_control_device_v1::Event::Finished => {
                if let Some(DataControlManager::Zwlr(dm)) = state.data_manager.as_ref() {
                    let source = dm.create_data_source(qh, ());
                    if let Some(DataControlDevice::Zwlr(dd)) = state.data_device.as_ref() {
                        dd.set_selection(Some(&source));
                    }
                }
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(offer) = id {
                    offer.destroy();
                }
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                let Some(_offer) = id else {
                    return;
                };
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
