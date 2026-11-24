use super::{App, Seat, Work, worker::Worker};
use crate::{Result, ipc, shm::Frame};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
};
use std::{
    fs::File,
    os::fd::{AsFd, OwnedFd},
    os::unix::fs::FileExt,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat,
        wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::{
    wp::viewporter::client::{wp_viewport, wp_viewporter},
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use xkb::keysyms;
use xkbcommon::xkb;

impl Frame {
    pub(super) fn buffer(
        &self,
        shm: &wl_shm::WlShm,
        abgr: bool,
        qh: &QueueHandle<App>,
    ) -> wl_buffer::WlBuffer {
        let format = if abgr {
            wl_shm::Format::Abgr8888
        } else {
            wl_shm::Format::Argb8888
        };
        let pool = shm.create_pool(self.file.as_fd(), self.bytes() as i32, qh, ());
        let buffer = pool.create_buffer(0, self.width, self.height, self.width * 4, format, qh, ());
        pool.destroy();
        buffer
    }
}

/// Implements `Dispatch` for `App` with a handler taking the state, proxy, event, user data and queue.
macro_rules! dispatch {
    ($proxy:ty, $data:ty, $handler:expr) => {
        impl Dispatch<$proxy, $data> for App {
            fn event(
                state: &mut Self,
                proxy: &$proxy,
                event: <$proxy as Proxy>::Event,
                data: &$data,
                _: &Connection,
                qh: &QueueHandle<Self>,
            ) {
                let handler: fn(
                    &mut Self,
                    &$proxy,
                    <$proxy as Proxy>::Event,
                    &$data,
                    &QueueHandle<Self>,
                ) = $handler;
                handler(state, proxy, event, data, qh);
            }
        }
    };
}
dispatch!(
    wl_registry::WlRegistry,
    (),
    |state, registry, event, _, qh| {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" if version >= 4 => {
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" if version >= 4 => {
                    state.wm_base = Some(registry.bind(name, 4, qh, ()))
                }
                "wp_viewporter" => state.viewporter = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if version >= 5 => {
                    state.seats.insert(
                        name,
                        Seat {
                            wl: registry.bind(name, 5, qh, name),
                            pointer: None,
                            keyboard: None,
                            pin: None,
                            key_pin: None,
                            position: (0.0, 0.0),
                            dragging: false,
                            keys: None,
                            repeat_rate: 0,
                            repeat_delay: 0,
                            repeating: None,
                        },
                    );
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(seat) = state.seats.remove(&name) {
                    if let Some(pointer) = seat.pointer {
                        pointer.release();
                    }
                    if let Some(keyboard) = seat.keyboard {
                        keyboard.release();
                    }
                    seat.wl.release();
                }
            }
            _ => {}
        }
    }
);

dispatch!(wl_shm::WlShm, (), |state, _, event, _, _| {
    if let wl_shm::Event::Format {
        format: WEnum::Value(wl_shm::Format::Abgr8888),
    } = event
    {
        state.abgr = true;
    }
});

dispatch!(xdg_wm_base::XdgWmBase, (), |_, wm, event, _, _| {
    if let xdg_wm_base::Event::Ping { serial } = event {
        wm.pong(serial);
    }
});

dispatch!(
    xdg_surface::XdgSurface,
    u64,
    |state, surface, event, id, _| {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Some(pin) = state.pins.get_mut(id).filter(|pin| !pin.configured) {
                pin.configured = true;
                pin.constraints();
                pin.work = Work::Render;
            }
        }
    }
);

dispatch!(xdg_toplevel::XdgToplevel, u64, |state, _, event, id, _| {
    match event {
        xdg_toplevel::Event::Close => state.close_pin(*id),
        xdg_toplevel::Event::ConfigureBounds { width, height } if width > 0 && height > 0 => {
            if let Some(pin) = state.pins.get_mut(id).filter(|pin| !pin.configured) {
                pin.bounds = (width, height);
                pin.initial_size();
            }
        }
        _ => {}
    }
});

dispatch!(wl_seat::WlSeat, u32, |state, _, event, name, qh| {
    let Some(seat) = state.seats.get_mut(name) else {
        return;
    };
    if let wl_seat::Event::Capabilities {
        capabilities: WEnum::Value(capabilities),
    } = event
    {
        if capabilities.contains(wl_seat::Capability::Pointer) {
            if seat.pointer.is_none() {
                seat.pointer = Some(seat.wl.get_pointer(qh, *name));
            }
        } else {
            if let Some(pointer) = seat.pointer.take() {
                pointer.release();
            }
            seat.pin = None;
            seat.dragging = false;
        }
        if capabilities.contains(wl_seat::Capability::Keyboard) {
            if seat.keyboard.is_none() {
                seat.keyboard = Some(seat.wl.get_keyboard(qh, *name));
            }
        } else {
            if let Some(keyboard) = seat.keyboard.take() {
                keyboard.release();
            }
            seat.key_pin = None;
            seat.keys = None;
            seat.repeating = None;
        }
    }
});

dispatch!(wl_pointer::WlPointer, u32, |state, _, event, name, _| {
    let Some(seat) = state.seats.get_mut(name) else {
        return;
    };
    match event {
        wl_pointer::Event::Enter {
            surface,
            surface_x,
            surface_y,
            ..
        } => {
            seat.pin = surface.data::<u64>().copied();
            seat.position = (surface_x, surface_y);
        }
        wl_pointer::Event::Leave { .. } => {
            seat.pin = None;
            seat.dragging = false;
        }
        wl_pointer::Event::Motion {
            surface_x,
            surface_y,
            ..
        } => {
            let old = seat.position;
            seat.position = (surface_x, surface_y);
            if seat.dragging
                && let Some(pin) = seat.pin.and_then(|id| state.pins.get_mut(&id))
            {
                pin.center.0 -= (surface_x - old.0) / pin.zoom;
                pin.center.1 -= (surface_y - old.1) / pin.zoom;
                pin.clamp_center();
                pin.invalidate();
            }
        }
        wl_pointer::Event::Button {
            serial,
            button,
            state: WEnum::Value(pressed),
            ..
        } => {
            if let Some(id) = seat.pin {
                if button == 0x110 {
                    if pressed == wl_pointer::ButtonState::Pressed {
                        if let Some(pin) = state.pins.get(&id) {
                            if pin.inspect {
                                seat.dragging = true;
                            } else {
                                pin.toplevel._move(&seat.wl, serial);
                            }
                        }
                    } else {
                        seat.dragging = false;
                    }
                } else if button == 0x111 && pressed == wl_pointer::ButtonState::Pressed {
                    state.close_pin(id);
                }
            }
        }
        wl_pointer::Event::Axis {
            axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
            value,
            ..
        } => {
            if let Some(pin) = seat.pin.and_then(|id| state.pins.get_mut(&id)) {
                pin.zoom(value);
            }
        }
        _ => {}
    }
});
dispatch!(wl_keyboard::WlKeyboard, u32, |state, _, event, name, _| {
    let Some(seat) = state.seats.get_mut(name) else {
        return;
    };
    match event {
        wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd,
            size,
        } => {
            seat.keys = None;
            seat.repeating = None;
            let mut bytes = vec![0; (size as usize).min(16 * 1024 * 1024)];
            if size <= 16 * 1024 * 1024
                && File::from(fd).read_exact_at(&mut bytes, 0).is_ok()
                && let Ok(text) = String::from_utf8(bytes)
            {
                let context = xkb::Context::new(xkb::CONTEXT_NO_DEFAULT_INCLUDES);
                if let Some(keymap) = xkb::Keymap::new_from_string(
                    &context,
                    text.trim_end_matches('\0').to_owned(),
                    xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkb::KEYMAP_COMPILE_NO_FLAGS,
                ) {
                    seat.keys = Some(xkb::State::new(&keymap));
                }
            }
        }
        wl_keyboard::Event::RepeatInfo { rate, delay } => {
            seat.repeat_rate = rate.max(0);
            seat.repeat_delay = delay.max(0);
            if rate <= 0 {
                seat.repeating = None;
            }
        }
        wl_keyboard::Event::Modifiers {
            mods_depressed,
            mods_latched,
            mods_locked,
            group,
            ..
        } => {
            if let Some(keys) = &mut seat.keys {
                keys.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
            }
        }
        wl_keyboard::Event::Enter { surface, .. } => {
            seat.repeating = None;
            seat.key_pin = surface.data::<u64>().copied();
        }
        wl_keyboard::Event::Leave { .. } => {
            seat.key_pin = None;
            seat.repeating = None;
        }
        wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(pressed),
            ..
        } => {
            if pressed == wl_keyboard::KeyState::Released {
                if seat.repeating.is_some_and(|(held, _, _)| held == key) {
                    seat.repeating = None;
                }
            } else if let Some(id) = seat.key_pin {
                let symbol = seat.keys.as_ref().and_then(|keys| {
                    key.checked_add(8).map(|code| {
                        (
                            keys.key_get_one_sym(code.into()).raw(),
                            keys.get_keymap().key_repeats(code.into()),
                        )
                    })
                });
                if let Some((symbol, repeats)) = symbol {
                    if repeats {
                        seat.repeating = None;
                    }
                    if matches!(symbol, keysyms::KEY_Left | keysyms::KEY_Right)
                        && repeats
                        && seat.repeat_rate > 0
                    {
                        seat.repeating = Some((
                            key,
                            symbol,
                            Instant::now() + Duration::from_millis(seat.repeat_delay as u64),
                        ));
                    }
                    state.key(id, symbol);
                }
            }
        }
        _ => {}
    }
});
dispatch!(wl_callback::WlCallback, u64, |state, callback, _, id, _| {
    if let Some(pin) = state.pins.get_mut(id)
        && let Some((current, start)) = &pin.callback
        && current == callback
    {
        pin.visible = start.elapsed() < Duration::from_millis(250);
        pin.callback = None;
    }
});

delegate_noop!(App: ignore wl_compositor::WlCompositor);
dispatch!(wl_surface::WlSurface, u64, |state, _, event, id, _| {
    if let wl_surface::Event::PreferredBufferScale { factor } = event
        && let Some(pin) = state.pins.get_mut(id)
        && pin.scale != factor.max(1)
    {
        pin.scale = factor.max(1);
        pin.invalidate();
    }
});
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wp_viewporter::WpViewporter);
delegate_noop!(App: ignore wp_viewport::WpViewport);

pub fn run() -> Result<()> {
    image_extras::register();
    let connection =
        Connection::connect_to_env().map_err(|e| format!("cannot connect to Wayland: {e}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut app = App::default();
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;
    if app.compositor.is_none()
        || app.shm.is_none()
        || app.wm_base.is_none()
        || app.viewporter.is_none()
        || app.seats.is_empty()
    {
        return Err("required Wayland protocols are unavailable".into());
    }
    app.worker = Some(Worker::new(!app.abgr)?);
    let server = ipc::server().map_err(|e| format!("cannot create daemon socket: {e}"))?;
    let mut clients = Vec::<OwnedFd>::new();
    loop {
        queue.dispatch_pending(&mut app)?;
        app.complete(&qh);
        let timeout = app.schedule(&qh);
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let writable = match connection.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                true
            }
            Err(e) => return Err(e.into()),
        };
        let mut fds = vec![
            PollFd::from_borrowed_fd(
                connection.as_fd(),
                PollFlags::IN
                    | if writable {
                        PollFlags::OUT
                    } else {
                        PollFlags::empty()
                    },
            ),
            PollFd::from_borrowed_fd(
                server.as_fd(),
                if clients.len() + app.pending.len() < ipc::MAX_CLIENTS {
                    PollFlags::IN
                } else {
                    PollFlags::empty()
                },
            ),
            PollFd::from_borrowed_fd(app.worker.as_ref().unwrap().wake.as_fd(), PollFlags::IN),
        ];
        let pending_ids: Vec<_> = app.pending.keys().copied().collect();
        fds.extend(
            clients
                .iter()
                .map(|client| PollFd::from_borrowed_fd(client.as_fd(), PollFlags::IN)),
        );
        fds.extend(
            app.pending
                .values()
                .map(|pending| PollFd::from_borrowed_fd(pending.fd.as_fd(), PollFlags::empty())),
        );
        let wait = timeout.map(Timespec::try_from).transpose()?;
        match poll(&mut fds, wait.as_ref()) {
            Err(Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let events: Vec<_> = fds.into_iter().map(|fd| fd.revents()).collect();
        let (client_events, pending_events) = events[3..].split_at(clients.len());
        let errors = PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL;
        if events[0].intersects(errors) {
            return Err("Wayland connection closed".into());
        }
        if events[1].intersects(errors) {
            return Err("daemon socket closed".into());
        }
        if events[2].intersects(errors) {
            return Err("image worker disconnected".into());
        }
        if events[0].contains(PollFlags::IN) {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            };
            queue.dispatch_pending(&mut app)?;
        } else {
            drop(guard);
        }
        for (id, event) in pending_ids.into_iter().zip(pending_events).rev() {
            if event.intersects(errors) {
                app.fail_request(id);
            }
        }
        for (index, event) in client_events.iter().enumerate().rev() {
            if !event.intersects(PollFlags::IN | errors) {
                continue;
            }
            let result = if event.intersects(PollFlags::ERR | PollFlags::NVAL) {
                Err("client disconnected".into())
            } else {
                ipc::receive(&clients[index])
            };
            match result {
                Ok(None) => {}
                Ok(Some(images)) => app.open_images(images, clients.remove(index)),
                Err(error) => {
                    eprintln!("perch: {error}");
                    let _ = ipc::reply(&clients.remove(index), false);
                }
            }
        }
        if events[1].contains(PollFlags::IN) {
            match ipc::accept(&server) {
                Ok(fd) => clients.push(fd),
                Err(Errno::AGAIN | Errno::INTR | Errno::ACCESS) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}
