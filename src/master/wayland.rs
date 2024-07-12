// Derived from https://github.com/Decodetalkers/wayland-clipboard-listener/blob/master/src/dispatch.rs

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
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

#[derive(Debug)]
pub(crate) struct ClipBoardListenMessage {
    pub mime_types: Vec<String>,
}

pub(crate) struct WlClipboardListener {
    seat: Option<wl_seat::WlSeat>,
    seat_name: Option<String>,
    data_manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    data_device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
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
                "Cannot get seat and data manager",
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
            (Some(seat), Some(manager)) => {
                let device = manager.get_data_device(seat, qh, ());
                self.data_device = Some(device);
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
            let read_guard = queue.prepare_read().map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("Prepare read failed: {e}"))
            })?;
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
                    }
                }
                Err(WaylandError::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
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
            mime_types: self.mime_types.clone(),
        })
    }
}

impl Iterator for WlClipboardListener {
    type Item = Result<ClipBoardListenMessage, io::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.get_message())
    }
}

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
                == zwlr_data_control_manager_v1::ZwlrDataControlManagerV1::interface().name
            {
                state.data_manager = Some(
                    registry.bind::<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _, _>(
                        name,
                        version,
                        qh,
                        (),
                    ),
                );
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
                if let Some(source) = state
                    .data_manager
                    .as_ref()
                    .map(|dm| dm.create_data_source(qh, ()))
                {
                    state
                        .data_device
                        .as_ref()
                        .map(|dd| dd.set_selection(Some(&source)));
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
