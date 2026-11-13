use crate::{
    Result,
    document::{Input, Source, View},
    image::Frame,
    ipc,
    loader::{self, Action, Done, Job, Output, Step, Token, Worker},
};
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
};
use std::{
    collections::BTreeMap,
    os::fd::{AsFd, OwnedFd},
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
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
impl Frame {
    fn buffer(
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
struct Seat {
    wl: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pin: Option<u64>,
    key_pin: Option<u64>,
    position: (f64, f64),
    dragging: bool,
}
struct Pin {
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    viewport: wp_viewport::WpViewport,
    buffer: Option<wl_buffer::WlBuffer>,
    source: Arc<Source>,
    paths: Arc<Vec<PathBuf>>,
    index: usize,
    loaded_index: usize,
    token: Token,
    size: (i32, i32),
    bounds: (i32, i32),
    zoom: f64,
    center: (f64, f64),
    inspect: bool,
    scale: i32,
    configured: bool,
    render_needed: bool,
    busy: bool,
    loading: bool,
    request: Option<u64>,
    playing: bool,
    ended: bool,
    next_frame: Instant,
    callback: Option<(u64, Instant)>,
    callback_serial: u64,
    visible: bool,
}
impl Pin {
    fn generation(&self) -> u64 {
        self.token.load(Ordering::Relaxed)
    }
    fn invalidate(&mut self) {
        if self.loading {
            return;
        }
        loader::cancel(&self.token);
        self.render_needed = true;
        self.busy = false;
    }
    fn title(&self) {
        let name = self
            .source
            .path()
            .and_then(|p| p.file_name())
            .map(|p| p.to_string_lossy())
            .unwrap_or_else(|| "stdin".into());
        let state = if self.loading {
            " [loading]"
        } else if self.playing {
            " [playing]"
        } else if self.inspect {
            " [inspect]"
        } else {
            ""
        };
        self.toplevel.set_title(format!("Perch: {name}{state}"));
    }
    fn fit(&self) -> f64 {
        (f64::from(self.bounds.0) * 0.9 / f64::from(self.source.size.0))
            .min(f64::from(self.bounds.1) * 0.9 / f64::from(self.source.size.1))
            .min(1.0)
    }
    fn initial_size(&mut self) {
        let fit = self.fit();
        self.size = (
            (f64::from(self.source.size.0) * fit).round().max(1.0) as i32,
            (f64::from(self.source.size.1) * fit).round().max(1.0) as i32,
        );
        self.inspect = self.source.region_only();
        self.zoom = if self.inspect { 1.0 } else { fit };
        self.center = (
            f64::from(self.source.size.0) / 2.0,
            f64::from(self.source.size.1) / 2.0,
        );
    }
    fn constraints(&self) {
        self.toplevel.set_min_size(self.size.0, self.size.1);
        self.toplevel.set_max_size(self.size.0, self.size.1);
    }
    fn clamp_center(&mut self) {
        let (sw, sh) = (f64::from(self.source.size.0), f64::from(self.source.size.1));
        let x = f64::from(self.size.0) / self.zoom / 2.0;
        let y = f64::from(self.size.1) / self.zoom / 2.0;
        self.center.0 = if x * 2.0 >= sw {
            sw / 2.0
        } else {
            self.center.0.clamp(x, sw - x)
        };
        self.center.1 = if y * 2.0 >= sh {
            sh / 2.0
        } else {
            self.center.1.clamp(y, sh - y)
        };
    }
    fn view(&self) -> View {
        let (mut width, mut height) = (
            f64::from(self.size.0) * f64::from(self.scale),
            f64::from(self.size.1) * f64::from(self.scale),
        );
        if !self.inspect {
            let ratio = (f64::from(self.source.size.0) / width)
                .min(f64::from(self.source.size.1) / height)
                .min(1.0);
            width *= ratio;
            height *= ratio;
        }
        let ratio = (4096.0 / width).min(4096.0 / height).min(1.0);
        width *= ratio;
        height *= ratio;
        let (left, top, span_width, span_height) = if self.inspect {
            (
                self.center.0 - f64::from(self.size.0) / self.zoom / 2.0,
                self.center.1 - f64::from(self.size.1) / self.zoom / 2.0,
                f64::from(self.size.0) / self.zoom,
                f64::from(self.size.1) / self.zoom,
            )
        } else {
            (
                0.0,
                0.0,
                f64::from(self.source.size.0),
                f64::from(self.source.size.1),
            )
        };
        View {
            width: width.round().max(1.0) as u32,
            height: height.round().max(1.0) as u32,
            left,
            top,
            span_width,
            span_height,
        }
    }
    fn zoom(&mut self, value: f64, position: (f64, f64)) {
        let minimum = if self.inspect {
            (f64::from(self.size.0) / f64::from(self.source.size.0))
                .min(f64::from(self.size.1) / f64::from(self.source.size.1))
                .max(self.source.min_zoom(self.size))
        } else {
            0.05_f64.min(self.fit())
        };
        let old = self.zoom;
        self.zoom = (old * 1.1_f64.powf(-value / 15.0)).clamp(minimum, 8.0_f64.max(minimum));
        if self.inspect {
            self.center.0 +=
                (position.0 - f64::from(self.size.0) / 2.0) * (1.0 / old - 1.0 / self.zoom);
            self.center.1 +=
                (position.1 - f64::from(self.size.1) / 2.0) * (1.0 / old - 1.0 / self.zoom);
            self.clamp_center();
        } else {
            self.size = (
                (f64::from(self.source.size.0) * self.zoom)
                    .round()
                    .clamp(1.0, f64::from(i32::MAX)) as i32,
                (f64::from(self.source.size.1) * self.zoom)
                    .round()
                    .clamp(1.0, f64::from(i32::MAX)) as i32,
            );
            self.constraints();
            if self.buffer.is_some() {
                self.viewport.set_destination(self.size.0, self.size.1);
                self.surface.commit();
            }
        }
        self.invalidate();
    }
    fn arm_callback(&mut self, id: u64, qh: &QueueHandle<App>) {
        self.callback_serial += 1;
        self.surface.frame(qh, (id, self.callback_serial));
        self.callback = Some((self.callback_serial, Instant::now()));
    }
}
impl Drop for Pin {
    fn drop(&mut self) {
        loader::cancel(&self.token);
        self.viewport.destroy();
        self.toplevel.destroy();
        self.xdg.destroy();
        self.surface.destroy();
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
    }
}
enum PendingState {
    Queued(Arc<Vec<Input>>),
    Decoding,
    Rendering(Vec<u64>),
}
struct Pending {
    fd: OwnedFd,
    token: Token,
    state: PendingState,
}
impl Drop for Pending {
    fn drop(&mut self) {
        loader::cancel(&self.token);
    }
}
#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    seats: BTreeMap<u32, Seat>,
    pins: BTreeMap<u64, Pin>,
    pending: BTreeMap<u64, Pending>,
    worker: Option<Worker>,
    next_id: u64,
    abgr: bool,
}
impl App {
    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    fn open_images(&mut self, images: ipc::Images, fd: OwnedFd) {
        let inputs = match images {
            ipc::Images::Paths(paths) => paths.into_iter().map(Input::Path).collect(),
            ipc::Images::Stdin(bytes) => vec![Input::Bytes(Arc::new(bytes))],
        };
        let id = self.id();
        self.pending.insert(
            id,
            Pending {
                fd,
                token: loader::token(),
                state: PendingState::Queued(Arc::new(inputs)),
            },
        );
    }
    fn create_pin(
        &mut self,
        source: Arc<Source>,
        paths: Arc<Vec<PathBuf>>,
        request: u64,
        qh: &QueueHandle<Self>,
    ) -> u64 {
        let id = self.id();
        let surface = self.compositor.as_ref().unwrap().create_surface(qh, id);
        let xdg = self
            .wm_base
            .as_ref()
            .unwrap()
            .get_xdg_surface(&surface, qh, id);
        let toplevel = xdg.get_toplevel(qh, id);
        let viewport = self
            .viewporter
            .as_ref()
            .unwrap()
            .get_viewport(&surface, qh, ());
        toplevel.set_app_id("perch".into());
        let index = source
            .path()
            .and_then(|path| paths.iter().position(|p| p == path))
            .unwrap_or(0);
        let mut pin = Pin {
            surface,
            xdg,
            toplevel,
            viewport,
            buffer: None,
            source,
            paths,
            index,
            loaded_index: index,
            token: loader::token(),
            size: (1, 1),
            bounds: (1920, 1080),
            zoom: 1.0,
            center: (0.0, 0.0),
            inspect: false,
            scale: 1,
            configured: false,
            render_needed: true,
            busy: false,
            loading: false,
            request: Some(request),
            playing: false,
            ended: false,
            next_frame: Instant::now(),
            callback: None,
            callback_serial: 0,
            visible: true,
        };
        pin.initial_size();
        pin.title();
        pin.surface.commit();
        self.pins.insert(id, pin);
        id
    }
    fn remove_pin(&mut self, id: u64) {
        self.pins.remove(&id);
        if let Some(worker) = &self.worker {
            worker.forget(id);
        }
        for seat in self.seats.values_mut() {
            if seat.pin == Some(id) {
                seat.pin = None;
                seat.dragging = false;
            }
            if seat.key_pin == Some(id) {
                seat.key_pin = None;
            }
        }
    }
    fn close_pin(&mut self, id: u64) {
        if let Some(request) = self.pins.get(&id).and_then(|p| p.request) {
            self.fail_request(request);
        } else {
            self.remove_pin(id);
        }
    }
    fn fail_request(&mut self, id: u64) {
        if let Some(pending) = self.pending.remove(&id) {
            let _ = ipc::send(&pending.fd, &[1]);
            if let PendingState::Rendering(ids) = &pending.state {
                for &pin in ids {
                    self.remove_pin(pin);
                }
            }
        }
    }
    fn preload(&self, id: u64) {
        let Some(pin) = self.pins.get(&id) else {
            return;
        };
        if pin.paths.len() < 2 {
            return;
        }
        let input = Input::Path(pin.paths[(pin.index + 1) % pin.paths.len()].clone());
        if let Some(worker) = &self.worker {
            worker.submit(Job {
                id,
                generation: pin.generation(),
                token: pin.token.clone(),
                action: Action::Preload(input),
            });
        }
    }
    fn navigate(&mut self, id: u64, direction: isize) {
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        if pin.paths.len() < 2 || pin.request.is_some() {
            return;
        }
        pin.index = (pin.index as isize + direction).rem_euclid(pin.paths.len() as isize) as usize;
        loader::cancel(&pin.token);
        pin.busy = false;
        pin.render_needed = false;
        pin.loading = true;
        pin.playing = false;
        pin.callback = None;
        pin.title();
        let input = Input::Path(pin.paths[pin.index].clone());
        // The queue retains only the latest request for this pin.
        if !self.worker.as_ref().unwrap().submit(Job {
            id,
            generation: pin.generation(),
            token: pin.token.clone(),
            action: Action::Load(input),
        }) {
            pin.loading = false;
            pin.index = pin.loaded_index;
            pin.title();
        }
    }
    fn key(&mut self, id: u64, key: u32) {
        if key == 105 || key == 106 {
            self.navigate(id, if key == 105 { -1 } else { 1 });
            return;
        }
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        if pin.loading {
            return;
        }
        match key {
            57 if pin.source.animated => {
                pin.playing = !pin.playing;
                pin.next_frame = Instant::now();
                pin.invalidate();
                pin.title();
            }
            15 => {
                if !pin.source.region_only() {
                    pin.inspect = !pin.inspect;
                    if !pin.inspect {
                        pin.zoom = f64::from(pin.size.0) / f64::from(pin.source.size.0);
                    }
                }
                pin.center = (
                    f64::from(pin.source.size.0) / 2.0,
                    f64::from(pin.source.size.1) / 2.0,
                );
                pin.invalidate();
                pin.title();
            }
            102 | 107 => {
                if pin.inspect {
                    pin.zoom = if key == 107 {
                        1.0
                    } else {
                        (f64::from(pin.size.0) / f64::from(pin.source.size.0))
                            .min(f64::from(pin.size.1) / f64::from(pin.source.size.1))
                    };
                    pin.zoom = pin.zoom.max(pin.source.min_zoom(pin.size));
                    pin.center = (
                        f64::from(pin.source.size.0) / 2.0,
                        f64::from(pin.source.size.1) / 2.0,
                    );
                    pin.clamp_center();
                } else {
                    pin.zoom = if key == 107 { 1.0 } else { pin.fit() };
                    pin.size = (
                        (f64::from(pin.source.size.0) * pin.zoom).round().max(1.0) as i32,
                        (f64::from(pin.source.size.1) * pin.zoom).round().max(1.0) as i32,
                    );
                    pin.constraints();
                }
                pin.invalidate();
            }
            _ => {}
        }
    }
    fn complete(&mut self, qh: &QueueHandle<Self>) {
        let done = self.worker.as_ref().unwrap().drain();
        for Done {
            id,
            generation,
            result,
        } in done
        {
            if self.pending.contains_key(&id) {
                match result {
                    Ok(Output::Opened { sources, paths }) => {
                        let ids = sources
                            .into_iter()
                            .map(|source| self.create_pin(source, paths.clone(), id, qh))
                            .collect();
                        self.pending.get_mut(&id).unwrap().state = PendingState::Rendering(ids);
                    }
                    Err(error) => {
                        eprintln!("perch: {error}");
                        self.fail_request(id);
                    }
                    _ => {}
                }
                continue;
            }
            let Some(pin) = self
                .pins
                .get_mut(&id)
                .filter(|p| p.generation() == generation)
            else {
                continue;
            };
            pin.busy = false;
            match result {
                Ok(Output::Loaded(source)) => {
                    pin.source = source;
                    pin.loaded_index = pin.index;
                    pin.loading = false;
                    pin.ended = false;
                    pin.initial_size();
                    pin.constraints();
                    pin.render_needed = true;
                    pin.title();
                }
                Ok(Output::Rendered {
                    frame,
                    delay,
                    stepped,
                    ended,
                }) => {
                    let buffer = frame.buffer(self.shm.as_ref().unwrap(), self.abgr, qh);
                    if let Some(old) = pin.buffer.replace(buffer.clone()) {
                        old.destroy();
                    }
                    pin.viewport.set_destination(pin.size.0, pin.size.1);
                    pin.surface.attach(Some(&buffer), 0, 0);
                    pin.surface.damage_buffer(0, 0, frame.width, frame.height);
                    if stepped {
                        pin.ended = ended;
                        if ended {
                            pin.playing = false;
                            pin.title();
                        }
                        if let Some(delay) = delay {
                            pin.next_frame = Instant::now() + delay;
                        }
                    }
                    if pin.playing && pin.callback.is_none() {
                        pin.arm_callback(id, qh);
                    }
                    pin.surface.commit();
                    if let Some(request) = pin.request {
                        let ready =
                            self.pending.get(&request).is_some_and(|pending| {
                                match &pending.state {
                                    PendingState::Rendering(ids) => ids.iter().all(|id| {
                                        self.pins.get(id).is_some_and(|pin| pin.buffer.is_some())
                                    }),
                                    _ => false,
                                }
                            });
                        if ready {
                            let pending = self.pending.remove(&request).unwrap();
                            if let PendingState::Rendering(ids) = &pending.state {
                                for id in ids {
                                    if let Some(pin) = self.pins.get_mut(id) {
                                        pin.request = None;
                                    }
                                }
                            }
                            let _ = ipc::send(&pending.fd, &[0]);
                        }
                    }
                    self.preload(id);
                }
                Err(error) => {
                    eprintln!("perch: {error}");
                    pin.loading = false;
                    pin.index = pin.loaded_index;
                    pin.playing = false;
                    pin.title();
                    if let Some(request) = pin.request {
                        self.fail_request(request);
                    }
                }
                _ => {}
            }
        }
    }
    fn schedule(&mut self, qh: &QueueHandle<Self>) -> Option<Duration> {
        let worker = self.worker.as_ref().unwrap();
        for (&id, pending) in &mut self.pending {
            if let PendingState::Queued(inputs) = &pending.state
                && worker.submit(Job {
                    id,
                    generation: 0,
                    token: pending.token.clone(),
                    action: Action::Open(inputs.clone()),
                })
            {
                pending.state = PendingState::Decoding;
            }
        }
        let now = Instant::now();
        let mut wait = None;
        for (&id, pin) in &mut self.pins {
            if !pin.configured || pin.loading {
                continue;
            }
            if pin.playing && !pin.visible && pin.callback.is_none() {
                pin.arm_callback(id, qh);
                pin.surface.commit();
            }
            let animate =
                pin.playing && pin.visible && pin.callback.is_none() && pin.next_frame <= now;
            if !pin.busy && (pin.render_needed || animate) {
                let step = if animate {
                    if pin.ended { Step::Restart } else { Step::Next }
                } else {
                    Step::Still
                };
                if worker.submit(Job {
                    id,
                    generation: pin.generation(),
                    token: pin.token.clone(),
                    action: Action::Render {
                        source: pin.source.clone(),
                        view: pin.view(),
                        step,
                    },
                }) {
                    pin.busy = true;
                    pin.render_needed = false;
                }
            }
            if pin.playing
                && pin.visible
                && !pin.busy
                && pin.callback.is_none()
                && pin.next_frame > now
            {
                let delay = pin.next_frame - now;
                wait = Some(wait.map_or(delay, |old: Duration| old.min(delay)));
            }
        }
        wait
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

impl Dispatch<xdg_surface::XdgSurface, u64> for App {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Some(pin) = state.pins.get_mut(id) {
                pin.configured = true;
                pin.constraints();
                pin.render_needed = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, u64> for App {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
                    pin.zoom(value, seat.position);
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, u32> for App {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(seat) = state.seats.get_mut(name) else {
            return;
        };
        match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                seat.key_pin = surface.data::<u64>().copied()
            }
            wl_keyboard::Event::Leave { .. } => seat.key_pin = None,
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => {
                if let Some(id) = seat.key_pin {
                    state.key(id, key);
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_callback::WlCallback, (u64, u64)> for App {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        (id, serial): &(u64, u64),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(pin) = state.pins.get_mut(id)
            && let Some((current, start)) = pin.callback
            && current == *serial
        {
            pin.callback = None;
            pin.visible = start.elapsed() < Duration::from_millis(250);
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
impl Dispatch<wl_surface::WlSurface, u64> for App {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event
            && let Some(pin) = state.pins.get_mut(id)
        {
            pin.scale = factor.max(1);
            pin.invalidate();
        }
    }
}
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wp_viewporter::WpViewporter);
delegate_noop!(App: ignore wp_viewport::WpViewport);

pub fn run() -> Result<()> {
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
    let mut clients = Vec::<ipc::Client>::new();
    #[derive(Clone, Copy)]
    enum Ready {
        Wayland,
        Server,
        Worker,
        Client(usize),
        Pending(u64),
    }
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
            PollFd::new(
                connection.as_fd(),
                PollFlags::POLLIN
                    | if writable {
                        PollFlags::POLLOUT
                    } else {
                        PollFlags::empty()
                    },
            ),
            PollFd::new(
                server.as_fd(),
                if clients.len() + app.pending.len() < ipc::MAX_CLIENTS {
                    PollFlags::POLLIN
                } else {
                    PollFlags::empty()
                },
            ),
            PollFd::new(app.worker.as_ref().unwrap().wake.as_fd(), PollFlags::POLLIN),
        ];
        let mut kinds = vec![Ready::Wayland, Ready::Server, Ready::Worker];
        for (i, client) in clients.iter().enumerate() {
            fds.push(PollFd::new(client.fd.as_fd(), PollFlags::POLLIN));
            kinds.push(Ready::Client(i));
        }
        for (&id, pending) in &app.pending {
            fds.push(PollFd::new(pending.fd.as_fd(), PollFlags::empty()));
            kinds.push(Ready::Pending(id));
        }
        let wait = timeout.map_or(PollTimeout::NONE, |d| {
            PollTimeout::try_from(d.as_millis().saturating_add(1)).unwrap_or(PollTimeout::MAX)
        });
        match poll(&mut fds, wait) {
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let events = fds
            .iter()
            .zip(kinds)
            .map(|(fd, kind)| (kind, fd.revents().unwrap_or(PollFlags::empty())))
            .collect::<Vec<_>>();
        drop(fds);
        let errors = PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL;
        if events[0].1.intersects(errors) || events[1].1.intersects(errors) {
            return Err("Wayland connection closed".into());
        }
        if events[2].1.intersects(errors) {
            return Err("image worker disconnected".into());
        }
        if events[0].1.contains(PollFlags::POLLIN) {
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
        for (kind, event) in events.iter().rev() {
            if !event.intersects(PollFlags::POLLIN | errors) {
                continue;
            }
            match *kind {
                Ready::Client(index) => {
                    let result = if event.intersects(PollFlags::POLLERR | PollFlags::POLLNVAL) {
                        Err("client disconnected".into())
                    } else {
                        clients[index].read()
                    };
                    match result {
                        Ok(None) => {}
                        Ok(Some(images)) => {
                            let client = clients.remove(index);
                            app.open_images(images, client.fd);
                        }
                        Err(error) => {
                            eprintln!("perch: {error}");
                            let client = clients.remove(index);
                            let _ = ipc::send(&client.fd, &[1]);
                        }
                    }
                }
                Ready::Pending(id) => app.fail_request(id),
                _ => {}
            }
        }
        if events[1].1.contains(PollFlags::POLLIN) {
            match ipc::accept(&server) {
                Ok(fd) => clients.push(ipc::Client::new(fd)),
                Err(Errno::EAGAIN | Errno::EINTR | Errno::EACCES) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}
