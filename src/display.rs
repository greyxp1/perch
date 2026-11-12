use crate::image::Pixels;
use crate::{Result, ipc};
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{BufReader, Cursor},
    os::{fd::AsFd, unix::fs::OpenOptionsExt},
    path::Path,
};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::{
    wp::viewporter::client::{wp_viewport, wp_viewporter},
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

impl Pixels {
    fn buffer(
        mut self,
        shm: &wl_shm::WlShm,
        abgr: bool,
        qh: &QueueHandle<App>,
    ) -> wl_buffer::WlBuffer {
        let format = if abgr {
            wl_shm::Format::Abgr8888
        } else {
            for pixel in self.mapping.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            wl_shm::Format::Argb8888
        };
        let pool = shm.create_pool(self.file.as_fd(), self.mapping.len() as i32, qh, ());
        let buffer = pool.create_buffer(0, self.width, self.height, self.width * 4, format, qh, ());
        pool.destroy();
        buffer
    }
}

struct Seat {
    wl: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
    pin: Option<usize>,
}

struct Pin {
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    viewport: wp_viewport::WpViewport,
    buffer: wl_buffer::WlBuffer,
    image_size: (i32, i32),
    size: (i32, i32),
    zoom: f64,
    min_zoom: f64,
    presented: bool,
    dirty: bool,
}

impl Pin {
    fn update_size(&mut self) {
        self.size = (
            (f64::from(self.image_size.0) * self.zoom)
                .round()
                .clamp(1.0, f64::from(i32::MAX)) as i32,
            (f64::from(self.image_size.1) * self.zoom)
                .round()
                .clamp(1.0, f64::from(i32::MAX)) as i32,
        );
        self.toplevel.set_min_size(self.size.0, self.size.1);
        self.toplevel.set_max_size(self.size.0, self.size.1);
        self.dirty = true;
    }

    fn render(&mut self) {
        if !self.dirty {
            return;
        }
        self.viewport.set_destination(self.size.0, self.size.1);
        if !self.presented {
            self.surface.attach(Some(&self.buffer), 0, 0);
            self.surface
                .damage_buffer(0, 0, self.image_size.0, self.image_size.1);
            self.presented = true;
        }
        self.surface.commit();
        self.dirty = false;
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        self.viewport.destroy();
        self.toplevel.destroy();
        self.xdg.destroy();
        self.surface.destroy();
        self.buffer.destroy();
    }
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    seats: BTreeMap<u32, Seat>,
    pins: BTreeMap<usize, Pin>,
    next_pin: usize,
    abgr: bool,
}

impl App {
    fn create_pin(&mut self, pixels: Pixels, qh: &QueueHandle<Self>) -> Result<()> {
        let (width, height) = (pixels.width, pixels.height);
        let buffer = pixels.buffer(self.shm.as_ref().ok_or("missing wl_shm")?, self.abgr, qh);
        self.pin_buffer(buffer, width, height, qh)
    }

    fn pin_buffer(
        &mut self,
        buffer: wl_buffer::WlBuffer,
        width: i32,
        height: i32,
        qh: &QueueHandle<Self>,
    ) -> Result<()> {
        let id = self.next_pin;
        self.next_pin += 1;
        let surface = self
            .compositor
            .as_ref()
            .ok_or("missing wl_compositor")?
            .create_surface(qh, id);
        let xdg = self
            .wm_base
            .as_ref()
            .ok_or("missing xdg_wm_base")?
            .get_xdg_surface(&surface, qh, id);
        let toplevel = xdg.get_toplevel(qh, id);
        let viewport = self
            .viewporter
            .as_ref()
            .ok_or("missing wp_viewporter")?
            .get_viewport(&surface, qh, ());
        toplevel.set_title("Perch".into());
        toplevel.set_app_id("perch".into());
        let pin = Pin {
            surface,
            xdg,
            toplevel,
            viewport,
            buffer,
            image_size: (width, height),
            size: (width, height),
            zoom: 1.0,
            min_zoom: 0.05,
            presented: false,
            dirty: true,
        };
        pin.surface.commit();
        self.pins.insert(id, pin);
        Ok(())
    }

    fn open_pin(&mut self, path: &Path, qh: &QueueHandle<Self>) -> Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err("expected a regular image file".into());
        }
        let pixels = Pixels::load(BufReader::new(file), Some(path.to_path_buf()))
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        self.create_pin(pixels, qh)
    }

    fn close_pin(&mut self, id: usize) {
        self.pins.remove(&id);
        for seat in self.seats.values_mut() {
            if seat.pin == Some(id) {
                seat.pin = None;
            }
        }
    }

    fn open_images(&mut self, images: ipc::Images, qh: &QueueHandle<Self>) -> Result<()> {
        match images {
            ipc::Images::Stdin(bytes) => {
                self.create_pin(Pixels::load(Cursor::new(bytes), None)?, qh)
            }
            ipc::Images::Paths(paths) => {
                let first = self.next_pin;
                for path in paths {
                    if let Err(error) = self.open_pin(&path, qh) {
                        drop(self.pins.split_off(&first));
                        return Err(error);
                    }
                }
                Ok(())
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" if version >= 4 => {
                    state.compositor = Some(registry.bind(name, 4, qh, ()))
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
                            pin: None,
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
                    seat.wl.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format {
            format: WEnum::Value(wl_shm::Format::Abgr8888),
        } = event
        {
            state.abgr = true;
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, usize> for App {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        id: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Some(pin) = state.pins.get_mut(id) {
                if !pin.presented {
                    pin.update_size();
                }
                pin.render();
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, usize> for App {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        id: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Close => state.close_pin(*id),
            xdg_toplevel::Event::ConfigureBounds { width, height } if width > 0 && height > 0 => {
                if let Some(pin) = state.pins.get_mut(id).filter(|pin| !pin.presented) {
                    pin.zoom = 1.0_f64
                        .min(f64::from(width) * 0.9 / f64::from(pin.image_size.0))
                        .min(f64::from(height) * 0.9 / f64::from(pin.image_size.1));
                    pin.min_zoom = 0.05_f64.min(pin.zoom);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for App {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
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
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, u32> for App {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(seat) = state.seats.get_mut(name) else {
            return;
        };
        match event {
            wl_pointer::Event::Enter { surface, .. } => seat.pin = surface.data::<usize>().copied(),
            wl_pointer::Event::Leave { .. } => {
                if let Some(pin) = seat.pin.take().and_then(|id| state.pins.get_mut(&id)) {
                    pin.render();
                }
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => {
                if let Some(id) = seat.pin {
                    if button == 0x110 {
                        if let Some(pin) = state.pins.get(&id) {
                            pin.toplevel._move(&seat.wl, serial);
                        }
                    } else if button == 0x111 {
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
                    pin.zoom = (pin.zoom * 1.1_f64.powf(-value / 15.0)).clamp(pin.min_zoom, 8.0);
                    pin.update_size();
                }
            }
            wl_pointer::Event::Frame => {
                if let Some(pin) = seat.pin.and_then(|id| state.pins.get_mut(&id)) {
                    pin.render();
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
impl Dispatch<wl_surface::WlSurface, usize> for App {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wp_viewporter::WpViewporter);
delegate_noop!(App: ignore wp_viewport::WpViewport);

pub fn run() -> Result<()> {
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("cannot connect to Wayland: {error}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut app = App::default();
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;
    if app.compositor.is_none()
        || app.shm.is_none()
        || app.seats.is_empty()
        || app.wm_base.is_none()
        || app.viewporter.is_none()
    {
        return Err("required Wayland protocols are unavailable".into());
    }
    let server = ipc::server().map_err(|error| format!("cannot create daemon socket: {error}"))?;
    let mut clients: Vec<ipc::Client> = Vec::new();
    loop {
        queue.dispatch_pending(&mut app)?;
        let Some(read_guard) = queue.prepare_read() else {
            continue;
        };
        let writable = match connection.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                true
            }
            Err(error) => return Err(error.into()),
        };
        let mut fds: [_; ipc::MAX_CLIENTS + 2] =
            std::array::from_fn(|_| PollFd::new(connection.as_fd(), PollFlags::empty()));
        fds[0] = PollFd::new(
            connection.as_fd(),
            PollFlags::POLLIN
                | if writable {
                    PollFlags::POLLOUT
                } else {
                    PollFlags::empty()
                },
        );
        fds[1] = PollFd::new(
            server.as_fd(),
            if clients.len() < ipc::MAX_CLIENTS {
                PollFlags::POLLIN
            } else {
                PollFlags::empty()
            },
        );
        for (fd, client) in fds[2..].iter_mut().zip(&clients) {
            *fd = PollFd::new(client.fd.as_fd(), PollFlags::POLLIN);
        }
        match poll(&mut fds[..clients.len() + 2], PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let events = fds.map(|fd| fd.revents().unwrap_or(PollFlags::empty()));
        let errors = PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL;
        if events[0].intersects(errors) || events[1].intersects(errors) {
            return Err("Wayland connection closed".into());
        }
        if events[0].contains(PollFlags::POLLIN) {
            match read_guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            queue.dispatch_pending(&mut app)?;
        } else {
            drop(read_guard);
        }
        for index in (0..clients.len()).rev() {
            let event = events[index + 2];
            if !event.intersects(PollFlags::POLLIN | errors) {
                continue;
            }
            let result = if event.intersects(PollFlags::POLLERR | PollFlags::POLLNVAL) {
                Err("client disconnected".into())
            } else {
                clients[index].read().and_then(|images| {
                    images
                        .map(|images| app.open_images(images, &qh))
                        .transpose()
                })
            };
            match result {
                Ok(None) => {}
                result => {
                    if let Err(error) = &result {
                        eprintln!("perch: {error}");
                    }
                    let _ = ipc::send(&clients[index].fd, &[u8::from(result.is_err())]);
                    clients.remove(index);
                }
            }
        }
        if events[1].contains(PollFlags::POLLIN) {
            match ipc::accept(&server) {
                Ok(fd) => clients.push(ipc::Client::new(fd)),
                Err(Errno::EAGAIN | Errno::EINTR | Errno::EACCES) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}
