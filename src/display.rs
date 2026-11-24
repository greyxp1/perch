use crate::{
    Result,
    document::{Input, Source, View},
    image::Frame,
    ipc,
    loader::{self, Action, Done, Job, Output, Step, Token, Worker},
};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
};
use std::{
    collections::BTreeMap,
    fs::File,
    os::fd::{AsFd, OwnedFd},
    os::unix::fs::FileExt,
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
use xkb::keysyms;
use xkbcommon::xkb;

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
    keys: Option<xkb::State>,
    repeat_rate: i32,
    repeat_delay: i32,
    repeating: Option<(u32, u32, Instant)>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Work {
    Idle,
    Render,
    Rendering,
    Loading,
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
    direction: isize,
    token: Token,
    size: (i32, i32),
    bounds: (i32, i32),
    zoom: f64,
    center: (f64, f64),
    inspect: bool,
    scale: i32,
    configured: bool,
    work: Work,
    request: Option<u64>,
    playing: bool,
    ended: bool,
    next_frame: Instant,
    callback: Option<(wl_callback::WlCallback, Instant)>,
    visible: bool,
}
impl Pin {
    fn neighbor(&self, direction: isize) -> usize {
        (self.index as isize + direction).rem_euclid(self.paths.len() as isize) as usize
    }
    fn generation(&self) -> u64 {
        self.token.load(Ordering::Relaxed)
    }
    fn invalidate(&mut self) {
        if self.work == Work::Loading {
            return;
        }
        loader::cancel(&self.token);
        self.work = Work::Render;
    }
    fn title(&self) {
        let name = self
            .source
            .path()
            .and_then(|p| p.file_name())
            .map(|p| p.to_string_lossy())
            .unwrap_or_else(|| "stdin".into());
        let state = if self.work == Work::Loading {
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
        self.fit_in(self.bounds, 0.9).min(1.0)
    }
    fn fit_in(&self, (w, h): (i32, i32), margin: f64) -> f64 {
        (f64::from(w) * margin / f64::from(self.source.size.0))
            .min(f64::from(h) * margin / f64::from(self.source.size.1))
    }
    fn center_image(&mut self) {
        self.center = (
            f64::from(self.source.size.0) / 2.0,
            f64::from(self.source.size.1) / 2.0,
        );
    }
    fn size_to_zoom(&mut self) {
        self.size = (
            (f64::from(self.source.size.0) * self.zoom).round().max(1.0) as i32,
            (f64::from(self.source.size.1) * self.zoom).round().max(1.0) as i32,
        );
    }
    fn initial_size(&mut self) {
        self.zoom = self.fit();
        self.size_to_zoom();
        self.inspect = false;
        self.center_image();
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
        let pixels = crate::RENDER_BYTES / 4;
        let ratio = (pixels as f64 / (width * height)).sqrt().min(1.0);
        width = (width * ratio).round().clamp(1.0, pixels as f64);
        height = (height * ratio)
            .round()
            .clamp(1.0, (pixels / width as usize) as f64);
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
            width: width as u32,
            height: height as u32,
            left,
            top,
            span_width,
            span_height,
        }
    }
    fn zoom(&mut self, value: f64) {
        let (minimum, maximum) = if self.inspect {
            (self.fit_in(self.size, 1.0), 8.0)
        } else {
            (
                0.05_f64.min(self.fit()),
                self.fit_in(self.bounds, 1.0).min(8.0),
            )
        };
        let zoom = (self.zoom * 1.1_f64.powf(-value / 15.0)).clamp(minimum, maximum.max(minimum));
        if zoom == self.zoom {
            return;
        }
        self.zoom = zoom;
        if self.inspect {
            self.clamp_center();
        } else {
            self.size_to_zoom();
            self.constraints();
            if self.buffer.is_some() {
                self.viewport.set_destination(self.size.0, self.size.1);
                self.surface.commit();
            }
        }
        self.invalidate();
    }
    fn arm_callback(&mut self, id: u64, qh: &QueueHandle<App>) {
        self.callback = Some((self.surface.frame(qh, id), Instant::now()));
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
    Queued(Arc<ipc::Request>),
    Decoding,
    // Member pins and how many still lack their first buffer.
    Rendering(Vec<u64>, usize),
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
    fn open_images(&mut self, request: ipc::Request, fd: OwnedFd) {
        let id = self.id();
        self.pending.insert(
            id,
            Pending {
                fd,
                token: loader::token(),
                state: PendingState::Queued(Arc::new(request)),
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
            direction: 1,
            token: loader::token(),
            size: (1, 1),
            bounds: (1920, 1080),
            zoom: 1.0,
            center: (0.0, 0.0),
            inspect: false,
            scale: 1,
            configured: false,
            work: Work::Render,
            request: Some(request),
            playing: false,
            ended: false,
            next_frame: Instant::now(),
            callback: None,
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
                seat.repeating = None;
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
            let _ = ipc::reply(&pending.fd, false);
            if let PendingState::Rendering(ids, _) = &pending.state {
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
        let input = Input::Path(pin.paths[pin.neighbor(pin.direction)].clone());
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
        // An idle pin can only have a speculative decode outstanding. Keep it
        // alive if this load will use its result.
        if pin.work != Work::Idle || pin.direction != direction {
            loader::cancel(&pin.token);
        }
        pin.index = pin.neighbor(direction);
        pin.direction = direction;
        pin.work = Work::Loading;
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
            pin.work = Work::Idle;
            pin.index = pin.loaded_index;
            pin.title();
        }
    }
    fn key(&mut self, id: u64, key: u32) {
        if key == keysyms::KEY_q || key == keysyms::KEY_Q {
            self.close_pin(id);
            return;
        }
        if key == keysyms::KEY_Left || key == keysyms::KEY_Right {
            self.navigate(id, if key == keysyms::KEY_Left { -1 } else { 1 });
            return;
        }
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        if pin.work == Work::Loading {
            return;
        }
        match key {
            keysyms::KEY_space if pin.source.playback.is_some() => {
                pin.playing = !pin.playing;
                pin.next_frame = Instant::now();
                pin.invalidate();
                pin.title();
            }
            keysyms::KEY_Tab => {
                pin.inspect = !pin.inspect;
                if !pin.inspect {
                    pin.zoom = pin.fit_in(pin.size, 1.0);
                }
                pin.center_image();
                pin.invalidate();
                pin.title();
            }
            keysyms::KEY_Home | keysyms::KEY_End => {
                if key == keysyms::KEY_End
                    && (u64::from(pin.source.size.0) > pin.bounds.0 as u64
                        || u64::from(pin.source.size.1) > pin.bounds.1 as u64)
                {
                    pin.inspect = true;
                    pin.title();
                }
                pin.zoom = if key == keysyms::KEY_End {
                    1.0
                } else if pin.inspect {
                    pin.fit_in(pin.size, 1.0)
                } else {
                    pin.fit()
                };
                if pin.inspect {
                    pin.center_image();
                    pin.clamp_center();
                } else {
                    pin.size_to_zoom();
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
                        let ids: Vec<_> = sources
                            .into_iter()
                            .map(|source| self.create_pin(source, paths.clone(), id, qh))
                            .collect();
                        let count = ids.len();
                        self.pending.get_mut(&id).unwrap().state =
                            PendingState::Rendering(ids, count);
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
            pin.work = Work::Idle;
            match result {
                Ok(Output::Loaded(source)) => {
                    pin.source = source;
                    pin.loaded_index = pin.index;
                    pin.ended = false;
                    pin.initial_size();
                    pin.constraints();
                    pin.work = Work::Render;
                    pin.title();
                }
                Ok(Output::Rendered {
                    frame,
                    delay,
                    stepped,
                }) => {
                    let buffer = frame.buffer(self.shm.as_ref().unwrap(), self.abgr, qh);
                    let old = pin.buffer.replace(buffer.clone());
                    let first = old.inspect(wl_buffer::WlBuffer::destroy).is_none();
                    pin.viewport.set_destination(pin.size.0, pin.size.1);
                    pin.surface.attach(Some(&buffer), 0, 0);
                    pin.surface.damage_buffer(0, 0, frame.width, frame.height);
                    if stepped {
                        pin.ended = delay.is_none();
                        if let Some(delay) = delay {
                            pin.next_frame = Instant::now() + delay;
                        } else {
                            pin.playing = false;
                            pin.title();
                        }
                    }
                    if pin.playing && pin.callback.is_none() {
                        pin.arm_callback(id, qh);
                    }
                    pin.surface.commit();
                    if first
                        && let Some(request) = pin.request
                        && let Some(Pending {
                            state: PendingState::Rendering(_, remaining),
                            ..
                        }) = self.pending.get_mut(&request)
                    {
                        *remaining -= 1;
                        if *remaining == 0 {
                            let pending = self.pending.remove(&request).unwrap();
                            if let PendingState::Rendering(ids, _) = &pending.state {
                                for id in ids {
                                    if let Some(pin) = self.pins.get_mut(id) {
                                        pin.request = None;
                                    }
                                }
                            }
                            let _ = ipc::reply(&pending.fd, true);
                        }
                    }
                    self.preload(id);
                }
                Err(error) => {
                    eprintln!("perch: {error}");
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
        let now = Instant::now();
        let repeats: Vec<_> = (self.seats.iter())
            .filter_map(|(&name, seat)| {
                let (_, key, deadline) = seat.repeating?;
                (deadline <= now).then_some((name, seat.key_pin?, key))
            })
            .collect();
        for (name, id, key) in repeats {
            if self.pins.get(&id).is_some_and(|pin| {
                pin.work == Work::Idle && pin.request.is_none() && pin.paths.len() > 1
            }) {
                let seat = self.seats.get_mut(&name).unwrap();
                seat.repeating.as_mut().unwrap().2 =
                    now + Duration::from_secs_f64(1.0 / f64::from(seat.repeat_rate));
                self.key(id, key);
            }
        }
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
        for (&id, pin) in &mut self.pins {
            if !pin.configured || pin.work == Work::Loading {
                continue;
            }
            if pin.playing && !pin.visible && pin.callback.is_none() {
                pin.arm_callback(id, qh);
                pin.surface.commit();
            }
            let animate =
                pin.playing && pin.visible && pin.callback.is_none() && pin.next_frame <= now;
            if pin.work == Work::Render || (pin.work == Work::Idle && animate) {
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
                    pin.work = Work::Rendering;
                }
            }
        }
        // Leave overdue repeats and frames pending while the worker finishes the current frame.
        let repeats = self
            .seats
            .values()
            .filter_map(|seat| Some(seat.repeating?.2));
        let frames = (self.pins.values())
            .filter(|pin| {
                pin.configured
                    && !matches!(pin.work, Work::Loading | Work::Rendering)
                    && pin.playing
                    && pin.visible
                    && pin.callback.is_none()
            })
            .map(|pin| pin.next_frame);
        repeats
            .chain(frames)
            .filter(|&deadline| deadline > now)
            .min()
            .map(|deadline| deadline - now)
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
