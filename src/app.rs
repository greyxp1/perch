use crate::{
    document::{Input, Source, View, fit_in, fit_zoom, scaled, view},
    ipc,
    shm::Frame,
};
use std::{
    collections::BTreeMap,
    os::fd::OwnedFd,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use wayland_client::{
    QueueHandle,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_seat, wl_shm, wl_surface,
    },
};
use wayland_protocols::{
    wp::viewporter::client::{wp_viewport, wp_viewporter},
    xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use worker::{Action, Done, Job, Output, Step, Token, Worker};
use xkb::keysyms;
use xkbcommon::xkb;

mod editor;
mod hud;
mod wayland;
mod worker;

pub use wayland::run;

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
    /// The latest key or button serial, which clipboard requests need.
    serial: u32,
    data_device: Option<wl_data_device::WlDataDevice>,
    /// The newest offer from other clients, kept only to destroy it once replaced.
    offer: Option<wl_data_offer::WlDataOffer>,
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
    editor: Option<Box<editor::Editor>>,
    /// The latest rendered frame, shown under the HUD and toast.
    base: Option<Arc<Frame>>,
    toast: Option<(String, Instant)>,
    /// Whether the HUD or toast changed since the last commit, and when it may next redraw.
    redraw: bool,
    next_redraw: Instant,
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
        worker::cancel(&self.token);
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
        } else if self.editing() {
            " [editing]"
        } else if self.inspect {
            " [inspect]"
        } else {
            ""
        };
        self.toplevel.set_title(format!("Perch: {name}{state}"));
    }
    fn fit(&self) -> f64 {
        fit_zoom(self.content(), self.bounds)
    }
    fn fit_in(&self, area: (i32, i32), margin: f64) -> f64 {
        fit_in(self.content(), area, margin)
    }
    fn center_image(&mut self) {
        self.center = (
            f64::from(self.source.size.0) / 2.0,
            f64::from(self.source.size.1) / 2.0,
        );
    }
    fn size_to_zoom(&mut self) {
        self.size = scaled(self.content(), self.zoom);
    }
    fn overlay(&self) -> bool {
        self.editing() || self.toast.is_some()
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
        let inspect = self.inspect.then_some((self.zoom, self.center));
        view(self.source.size, self.size, self.scale, inspect)
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
        worker::cancel(&self.token);
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
        worker::cancel(&self.token);
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
    data_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    /// The selection Perch offers until another client replaces it.
    clipboard: Option<wl_data_source::WlDataSource>,
    labels: hud::Labels,
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
                token: worker::token(),
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
            token: worker::token(),
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
            editor: None,
            base: None,
            toast: None,
            redraw: false,
            next_redraw: Instant::now(),
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
        // Up to three distinct neighbors in the navigation direction, nearest first.
        let paths = (1..pin.paths.len().min(4))
            .map(|step| pin.paths[pin.neighbor(pin.direction * step as isize)].clone())
            .collect();
        let screen = (pin.bounds, pin.scale);
        (self.worker.as_ref().unwrap()).ahead(id, &pin.token, paths, screen);
    }
    fn navigate(&mut self, id: u64, direction: isize) {
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        if pin.paths.len() < 2 || pin.request.is_some() || pin.editor.is_some() {
            return;
        }
        let index = pin.neighbor(direction);
        let path = pin.paths[index].clone();
        self.load(id, index, direction, path);
    }
    /// Loads the image an overwrite replaced.
    fn reload(&mut self, id: u64) {
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        let Some(path) = pin.source.path().map(PathBuf::from) else {
            return;
        };
        pin.close_editor();
        let (index, direction) = (pin.index, pin.direction);
        self.load(id, index, direction, path);
    }
    fn load(&mut self, id: u64, index: usize, direction: isize, path: PathBuf) {
        let pin = self.pins.get_mut(&id).unwrap();
        // An idle pin can only have speculative work outstanding. Keep it alive
        // if this load or the following ones will use its results.
        if pin.work != Work::Idle || pin.direction != direction {
            worker::cancel(&pin.token);
        }
        pin.index = index;
        pin.direction = direction;
        pin.work = Work::Loading;
        pin.playing = false;
        pin.callback = None;
        pin.title();
        let input = Input::Path(path);
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
            self.preload(id);
        }
    }
    fn key(
        &mut self,
        id: u64,
        key: u32,
        (ctrl, shift): (bool, bool),
        seat: u32,
        qh: &QueueHandle<Self>,
    ) {
        // Letters match without case so Caps Lock never turns a save into an overwrite.
        let key = if (keysyms::KEY_A..=keysyms::KEY_Z).contains(&key) {
            key + (keysyms::KEY_a - keysyms::KEY_A)
        } else {
            key
        };
        match key {
            keysyms::KEY_c if ctrl => return self.copy(id, seat, qh),
            keysyms::KEY_s if ctrl => return self.save(id, shift),
            keysyms::KEY_z if ctrl => return self.undo(id, shift),
            keysyms::KEY_y if ctrl => return self.undo(id, true),
            _ => {}
        }
        if key == keysyms::KEY_q {
            self.close_pin(id);
            return;
        }
        if key == keysyms::KEY_e {
            self.toggle_edit(id);
            return;
        }
        if self.editing(id) && self.edit_key(id, key, shift) {
            return;
        }
        if key == keysyms::KEY_Left || key == keysyms::KEY_Right {
            self.navigate(id, if key == keysyms::KEY_Left { -1 } else { 1 });
            return;
        }
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        if pin.work == Work::Loading || pin.editor.is_some() {
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
                Ok(Output::Loaded(source, frame)) => {
                    pin.source = source;
                    pin.loaded_index = pin.index;
                    pin.ended = false;
                    pin.initial_size();
                    pin.constraints();
                    pin.title();
                    match frame {
                        Some((view, frame)) if view == pin.view() => {
                            self.present(id, frame, None, false, qh)
                        }
                        _ => pin.work = Work::Render,
                    }
                }
                Ok(Output::Rendered {
                    frame,
                    delay,
                    stepped,
                }) => self.present(id, frame, delay, stepped, qh),
                Err(error) => {
                    eprintln!("perch: {error}");
                    pin.index = pin.loaded_index;
                    pin.playing = false;
                    pin.title();
                    if let Some(request) = pin.request {
                        self.fail_request(request);
                    }
                    // Replanning evicts entries kept for a direction the failed load reversed.
                    self.preload(id);
                }
                _ => {}
            }
        }
        for apply in self.worker.as_ref().unwrap().finished() {
            apply(self, qh);
        }
    }
    /// Attaches the latest frame, with the HUD and toast drawn over it when shown.
    fn attach(&mut self, id: u64, qh: &QueueHandle<Self>) {
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        let Some(base) = pin.base.clone() else {
            return;
        };
        let frame = if pin.overlay() {
            hud::compose(pin, &base, &mut self.labels).unwrap_or_else(|error| {
                eprintln!("perch: {error}");
                base
            })
        } else {
            base
        };
        let buffer = frame.buffer(self.shm.as_ref().unwrap(), self.abgr, qh);
        if let Some(old) = pin.buffer.replace(buffer.clone()) {
            old.destroy();
        }
        pin.viewport.set_destination(pin.size.0, pin.size.1);
        pin.surface.attach(Some(&buffer), 0, 0);
        pin.surface.damage_buffer(0, 0, frame.width, frame.height);
        pin.redraw = false;
        // Pointer motion arrives far faster than displays refresh.
        pin.next_redraw = Instant::now() + Duration::from_millis(8);
    }
    fn present(
        &mut self,
        id: u64,
        frame: Arc<Frame>,
        delay: Option<Duration>,
        stepped: bool,
        qh: &QueueHandle<Self>,
    ) {
        let pin = self.pins.get_mut(&id).unwrap();
        let first = pin.buffer.is_none();
        pin.base = Some(frame);
        self.attach(id, qh);
        let pin = self.pins.get_mut(&id).unwrap();
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
                self.key(id, key, (false, false), name, qh);
            }
        }
        for pin in self.pins.values_mut() {
            if pin.toast.as_ref().is_some_and(|(_, until)| *until <= now) {
                pin.toast = None;
                pin.redraw = true;
            }
        }
        let redraws: Vec<_> = (self.pins.iter())
            .filter(|(_, pin)| pin.redraw && pin.base.is_some() && pin.next_redraw <= now)
            .map(|(&id, _)| id)
            .collect();
        for id in redraws {
            self.attach(id, qh);
            self.pins[&id].surface.commit();
        }
        let swap = !self.abgr;
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
            if pin.editor.is_some() {
                editor::schedule(id, pin, worker, swap);
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
        let overlays = self.pins.values().flat_map(|pin| {
            let redraw = (pin.redraw && pin.base.is_some()).then_some(pin.next_redraw);
            redraw
                .into_iter()
                .chain(pin.toast.as_ref().map(|(_, until)| *until))
        });
        repeats
            .chain(frames)
            .chain(overlays)
            .filter(|&deadline| deadline > now)
            .min()
            .map(|deadline| deadline - now)
    }
}
