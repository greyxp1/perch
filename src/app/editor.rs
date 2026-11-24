//! Edit mode of a pin: tools, background previews, size estimates, saving and copying.
use super::{App, Pin, Work, hud, worker::Worker};
use crate::{
    document::Source,
    edit::{EditState, History},
    encode::{
        self, CompressOptions, ConvertOptions, Format, Raster,
        cli::{size, write},
    },
    shm::{Frame, Pixels},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};
use wayland_client::QueueHandle;
use xkbcommon::xkb::keysyms;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tool {
    Crop,
    Tone,
    Resize,
    Export,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Control {
    Tool(Tool),
    Rotate,
    FlipH,
    FlipV,
    Undo,
    Redo,
    Reset,
    Tone(usize),
    Scale(u8),
    Step(bool),
    Format(Option<Format>),
    Lossy(bool),
    Quality,
    Keep(bool),
    Save,
    Overwrite,
}
/// Whether the export format offers lossy encoding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Lossy {
    Never,
    Choice,
    Always,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Export {
    /// `None` keeps the source format.
    pub(super) format: Option<Format>,
    pub(super) lossy: bool,
    pub(super) quality: u8,
    pub(super) keep: bool,
}
impl Default for Export {
    fn default() -> Self {
        Self {
            format: None,
            lossy: false,
            quality: 85,
            keep: false,
        }
    }
}
type Key = (EditState, Export);
pub(super) struct Estimate {
    before: usize,
    /// `None` when compressing would not shrink the source.
    after: Option<Arc<Vec<u8>>>,
}
#[derive(Clone, Copy)]
enum Drag {
    /// The slider with its track, and whether tone history still needs the pre-drag state.
    Slider(Control, hud::Rect, bool),
    /// The grabbed edges, or `None` for a new box, with the image point and box at the press.
    Crop(Option<(i8, i8)>, (f64, f64), [f64; 4]),
}

/// The encoded source and its full-resolution pixels, loaded once by the first task needing them.
pub(super) struct Doc {
    source: Arc<Source>,
    cell: OnceLock<Result<Original, String>>,
}
pub(super) struct Original {
    bytes: Vec<u8>,
    raster: Raster,
    format: Option<Format>,
}
impl Doc {
    fn get(&self) -> Result<&Original, String> {
        let load = || -> crate::Result<Original> {
            let bytes = self.source.read()?;
            let raster = Raster::load(&bytes, self.source.path())?;
            if (raster.width, raster.height) != self.source.size {
                return Err("the image changed while open".into());
            }
            Ok(Original {
                format: Format::detect(&bytes),
                bytes,
                raster,
            })
        };
        let result = self.cell.get_or_init(|| load().map_err(|e| e.to_string()));
        result.as_ref().map_err(Clone::clone)
    }
    fn loaded(&self) -> Option<&Original> {
        self.cell.get()?.as_ref().ok()
    }
}

type Cached = (EditState, (u32, u32), Raster);
pub(super) struct Editor {
    serial: u64,
    doc: Arc<Doc>,
    /// Size of the unedited image, which edit coordinates refer to.
    pub(super) size: (u32, u32),
    pub(super) history: History,
    pub(super) active: bool,
    pub(super) tool: Option<Tool>,
    /// Corners of the crop box in pixels of the rotated image while cropping.
    crop: [f64; 4],
    drag: Option<Drag>,
    pub(super) hover: Option<Control>,
    pub(super) export: Export,
    estimate: Option<(Key, Result<Estimate, String>)>,
    estimating: bool,
    previewing: bool,
    /// The last geometry preview, reused while only tone changes.
    cache: Arc<Mutex<Option<Cached>>>,
    /// Zoom, size, inspection and center to restore when leaving without edits.
    restore: (f64, (i32, i32), bool, (f64, f64)),
    pub(super) min: (i32, i32),
}
impl Editor {
    fn key(&self) -> Key {
        (self.history.current, self.export)
    }
    fn compressing(&self) -> bool {
        self.export.format.is_none() && self.history.current.is_identity()
    }
    fn source_format(&self) -> Option<Format> {
        self.doc.loaded().and_then(|o| o.format)
    }
    pub(super) fn lossy(&self) -> Lossy {
        use Format::*;
        if self.compressing() {
            return match self.source_format() {
                Some(Png | Jpeg | WebP | Avif | Jxl) => Lossy::Choice,
                _ => Lossy::Never,
            };
        }
        match self.export.format.or(self.source_format()) {
            Some(Jpeg) => Lossy::Always,
            Some(WebP | Avif | Jxl) => Lossy::Choice,
            _ => Lossy::Never,
        }
    }
    /// Whether overwriting is possible, or `None` without a file on disk.
    pub(super) fn can_overwrite(&self) -> Option<bool> {
        self.doc.source.path()?;
        let source = self.source_format();
        Some(source.is_some() && self.export.format.is_none_or(|f| Some(f) == source))
    }
    pub(super) fn estimate_text(&self) -> String {
        match &self.estimate {
            Some((key, result)) if *key == self.key() => match result {
                Ok(Estimate {
                    before,
                    after: Some(after),
                }) => format!("{} → {}", size(*before), size(after.len())),
                Ok(_) => "Already optimal".into(),
                Err(error) if error.chars().count() > 48 => {
                    format!("{}…", error.chars().take(47).collect::<String>())
                }
                Err(error) => error.clone(),
            },
            _ => "…".into(),
        }
    }
    fn scaled(&self, percent: u8) -> Option<(u32, u32)> {
        let (w, h) = self.history.current.cropped(self.size);
        let side = |n: u32| ((f64::from(n) * f64::from(percent) / 100.0).round() as u32).max(1);
        (percent != 100).then(|| (side(w), side(h)))
    }
    /// The resize preset in effect.
    pub(super) fn percent(&self) -> Option<u8> {
        [75, 50, 25, 100]
            .into_iter()
            .find(|&p| self.scaled(p) == self.history.current.resize)
    }
    fn image_size(&self) -> (f64, f64) {
        let (w, h) = self.history.current.oriented(self.size);
        (f64::from(w), f64::from(h))
    }
    fn end_drag(&mut self) {
        // A click without dragging keeps the previous box.
        if let Some(Drag::Crop(None, _, start)) = self.drag.take() {
            let [x0, y0, x1, y1] = self.crop;
            if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
                self.crop = start;
            }
        }
    }
}

impl Pin {
    pub(super) fn editing(&self) -> bool {
        self.editor.as_ref().is_some_and(|e| e.active)
    }
    /// Size of what the pin shows, after edits.
    pub(super) fn content(&self) -> (u32, u32) {
        match self.editor.as_deref() {
            Some(e) if e.tool == Some(Tool::Crop) => e.history.current.oriented(self.source.size),
            Some(e) => e.history.current.output(self.source.size),
            None => self.source.size,
        }
    }
    /// Buffer pixels the content fills, centered in edit mode windows sized for the HUD.
    pub(super) fn content_px(&self) -> (i32, i32, i32, i32) {
        let (bw, bh) = (self.size.0 * self.scale, self.size.1 * self.scale);
        if self.editor.is_none() {
            return (0, 0, bw, bh);
        }
        let (cw, ch) = self.content();
        let fit = (f64::from(bw) / f64::from(cw)).min(f64::from(bh) / f64::from(ch));
        let w = ((f64::from(cw) * fit).round() as i32).clamp(1, bw);
        let h = ((f64::from(ch) * fit).round() as i32).clamp(1, bh);
        ((bw - w) / 2, (bh - h) / 2, w, h)
    }
    pub(super) fn content_rect(&self) -> hud::Rect {
        let (x, y, w, h) = self.content_px();
        let scale = self.scale as f32;
        (
            x as f32 / scale,
            y as f32 / scale,
            w as f32 / scale,
            h as f32 / scale,
        )
    }
    /// The crop box in window coordinates while cropping.
    pub(super) fn crop_rect(&self) -> Option<hud::Rect> {
        let editor = self
            .editor
            .as_deref()
            .filter(|e| e.tool == Some(Tool::Crop))?;
        let (x, y, w, h) = self.content_rect();
        let (iw, ih) = editor.image_size();
        let [x0, y0, x1, y1] = editor.crop.map(|v| v as f32);
        let (sx, sy) = (w / iw as f32, h / ih as f32);
        Some((x + x0 * sx, y + y0 * sy, (x1 - x0) * sx, (y1 - y0) * sy))
    }
    /// Maps a window point to pixels of the rotated image, clamped to it.
    fn image_point(&self, (px, py): (f64, f64)) -> (f64, f64) {
        let (x, y, w, h) = self.content_rect();
        let (iw, ih) = self
            .editor
            .as_deref()
            .map_or((1.0, 1.0), Editor::image_size);
        (
            ((px - f64::from(x)) * iw / f64::from(w)).clamp(0.0, iw),
            ((py - f64::from(y)) * ih / f64::from(h)).clamp(0.0, ih),
        )
    }
    /// Sizes the window to fit the content, leaving room for the HUD in edit mode.
    pub(super) fn fit_window(&mut self) {
        self.zoom = self.fit();
        self.size_to_zoom();
        self.inspect = false;
        if let Some(editor) = self.editor.as_deref().filter(|e| e.active) {
            self.size.0 = self.size.0.max(editor.min.0.min(self.bounds.0));
            self.size.1 = self.size.1.max(editor.min.1.min(self.bounds.1));
        }
        self.resized();
    }
    fn resized(&mut self) {
        self.constraints();
        if self.buffer.is_some() {
            self.viewport.set_destination(self.size.0, self.size.1);
            self.redraw = true;
        }
        self.invalidate();
    }
    /// Leaves edit mode, dropping the editor when nothing was edited.
    fn exit_edit(&mut self) {
        let Some(editor) = self.editor.as_deref_mut() else {
            return;
        };
        (editor.active, editor.tool, editor.drag, editor.hover) = (false, None, None, None);
        if editor.history.current.is_identity() {
            self.close_editor();
        } else {
            self.fit_window();
        }
        self.title();
    }
    /// Discards the editor and restores the view from before editing.
    pub(super) fn close_editor(&mut self) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        (self.zoom, self.size, self.inspect, self.center) = editor.restore;
        self.redraw = true;
        self.resized();
    }
    fn start_crop(&mut self) {
        let Some(editor) = self.editor.as_deref_mut() else {
            return;
        };
        let (w, h) = editor.image_size();
        editor.crop = editor
            .history
            .current
            .crop
            .map_or([0.0, 0.0, w, h], |(x, y, cw, ch)| {
                [x, y, x + cw, y + ch].map(f64::from)
            });
        editor.tool = Some(Tool::Crop);
        self.fit_window();
    }
    /// Leaves the crop tool, applying the box when `commit` is set.
    fn end_crop(&mut self, commit: bool) {
        let Some(editor) = self.editor.as_deref_mut() else {
            return;
        };
        editor.tool = None;
        editor.end_drag();
        if commit {
            let (w, h) = editor.history.current.oriented(editor.size);
            let [x0, y0, x1, y1] = editor.crop.map(|v| v.round() as u32);
            let (x0, y0) = (x0.min(w.saturating_sub(1)), y0.min(h.saturating_sub(1)));
            let rect = (
                x0,
                y0,
                x1.saturating_sub(x0).max(1),
                y1.saturating_sub(y0).max(1),
            );
            let size = editor.size;
            editor.history.change(|s| s.set_crop(size, Some(rect)));
        }
        self.fit_window();
    }
    /// Applies one undoable edit, first applying an open crop box.
    fn change(&mut self, edit: impl FnOnce(&mut EditState, (u32, u32))) {
        if self
            .editor
            .as_ref()
            .is_some_and(|e| e.tool == Some(Tool::Crop))
        {
            self.end_crop(true);
        }
        let Some(editor) = self.editor.as_deref_mut() else {
            return;
        };
        let size = editor.size;
        if editor.history.change(|s| edit(s, size)) {
            self.fit_window();
        }
        self.redraw = true;
    }
    fn history(&mut self, step: fn(&mut History) -> bool) {
        if self
            .editor
            .as_ref()
            .is_some_and(|e| e.tool == Some(Tool::Crop))
        {
            self.end_crop(false);
        }
        if let Some(editor) = self.editor.as_deref_mut()
            && step(&mut editor.history)
        {
            self.fit_window();
        }
        self.redraw = true;
    }
    fn control(&mut self, control: Control) {
        let Some(editor) = self.editor.as_deref_mut() else {
            return;
        };
        let tool = editor.tool;
        match control {
            Control::Tool(Tool::Crop) if tool == Some(Tool::Crop) => self.end_crop(true),
            Control::Tool(Tool::Crop) => self.start_crop(),
            Control::Tool(next) => {
                if tool == Some(Tool::Crop) {
                    self.end_crop(true);
                }
                let editor = self.editor.as_deref_mut().unwrap();
                editor.tool = (tool != Some(next)).then_some(next);
            }
            Control::Rotate => self.change(|s, size| s.rotate(size, true)),
            Control::FlipH => self.change(|s, size| s.flip(size, true)),
            Control::FlipV => self.change(|s, size| s.flip(size, false)),
            Control::Undo => self.history(History::undo),
            Control::Redo => self.history(History::redo),
            Control::Reset => self.history(History::reset),
            Control::Scale(percent) => {
                let resize = editor.scaled(percent);
                self.change(|s, _| s.resize = resize);
            }
            Control::Step(up) => {
                let (cropped, output) = (
                    editor.history.current.cropped(editor.size),
                    editor.history.current.output(editor.size),
                );
                let percent = f64::from(output.0) * 100.0 / f64::from(cropped.0);
                let step = (percent / 5.0).round() * 5.0 + if up { 5.0 } else { -5.0 };
                let resize = editor.scaled(step.clamp(5.0, 200.0) as u8);
                self.change(|s, _| s.resize = resize);
            }
            Control::Format(format) => editor.export.format = format,
            Control::Lossy(lossy) => editor.export.lossy = lossy,
            Control::Keep(keep) => editor.export.keep = keep,
            Control::Tone(_) | Control::Quality | Control::Save | Control::Overwrite => {}
        }
        self.redraw = true;
    }
    /// Sets the dragged slider from the pointer.
    fn slide(&mut self, pointer: f32) {
        let Some(editor) = self.editor.as_deref_mut() else {
            return;
        };
        let Some(Drag::Slider(control, (x, _, w, _), pending)) = &mut editor.drag else {
            return;
        };
        let fraction = ((pointer - *x - 6.0) / (*w - 12.0)).clamp(0.0, 1.0);
        match *control {
            Control::Tone(index) => {
                let mut state = editor.history.current;
                *match index {
                    0 => &mut state.brightness,
                    1 => &mut state.contrast,
                    _ => &mut state.saturation,
                } = (fraction * 200.0 - 100.0).round() as i8;
                if state != editor.history.current {
                    // A drag is one undo step, recorded once it changes something.
                    if std::mem::take(pending) {
                        editor.history.checkpoint();
                    }
                    editor.history.current = state;
                    self.invalidate();
                }
            }
            Control::Quality => editor.export.quality = (1.0 + fraction * 99.0).round() as u8,
            _ => {}
        }
        self.redraw = true;
    }
}

/// Moves the crop box corners `start` as the pointer goes from `from` to `to`.
fn drag_crop(
    handle: Option<(i8, i8)>,
    from: (f64, f64),
    start: [f64; 4],
    to: (f64, f64),
    (w, h): (f64, f64),
) -> [f64; 4] {
    let [x0, y0, x1, y1] = start;
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    match handle {
        None => [
            from.0.min(to.0),
            from.1.min(to.1),
            from.0.max(to.0),
            from.1.max(to.1),
        ],
        Some((0, 0)) => {
            let (dx, dy) = (dx.clamp(-x0, w - x1), dy.clamp(-y0, h - y1));
            [x0 + dx, y0 + dy, x1 + dx, y1 + dy]
        }
        Some((hx, hy)) => {
            let mut rect = start;
            match hx {
                -1 => rect[0] = (x0 + dx).clamp(0.0, x1 - 1.0),
                1 => rect[2] = (x1 + dx).clamp(x0 + 1.0, w),
                _ => {}
            }
            match hy {
                -1 => rect[1] = (y0 + dy).clamp(0.0, y1 - 1.0),
                1 => rect[3] = (y1 + dy).clamp(y0 + 1.0, h),
                _ => {}
            }
            rect
        }
    }
}

/// Starts previews and size estimates an editing pin needs.
pub(super) fn schedule(id: u64, pin: &mut Pin, worker: &Worker, swap: bool) {
    let (_, _, w, h) = pin.content_px();
    let render = pin.configured && pin.work == Work::Render;
    let Some(editor) = pin.editor.as_deref_mut() else {
        return;
    };
    if render && !editor.previewing {
        let (doc, cache, serial) = (editor.doc.clone(), editor.cache.clone(), editor.serial);
        let mut edit = editor.history.current;
        if editor.tool == Some(Tool::Crop) {
            (edit.crop, edit.resize) = (None, None);
        }
        let size = (w as u32, h as u32);
        editor.previewing = worker.spawn(move || {
            let result = preview(&doc, &cache, edit, size, swap).map_err(|e| e.to_string());
            Box::new(move |app: &mut App, qh: &QueueHandle<App>| {
                app.previewed(id, serial, result, qh)
            })
        });
        pin.work = Work::Idle;
    }
    if editor.active
        && editor.tool == Some(Tool::Export)
        && !editor.estimating
        && editor
            .estimate
            .as_ref()
            .is_none_or(|(k, _)| *k != editor.key())
    {
        let (doc, key, serial) = (editor.doc.clone(), editor.key(), editor.serial);
        editor.estimating = worker.spawn(move || {
            let result = doc.get().and_then(|original| {
                let after = encode_with(original, key).map_err(|e| e.to_string())?;
                Ok(Estimate {
                    before: original.bytes.len(),
                    after: after.map(Arc::new),
                })
            });
            Box::new(move |app: &mut App, _: &QueueHandle<App>| {
                if let Some(pin) = app.pins.get_mut(&id)
                    && let Some(editor) = pin.editor.as_deref_mut()
                    && editor.serial == serial
                {
                    editor.estimating = false;
                    editor.estimate = Some((key, result));
                    pin.redraw = true;
                }
            })
        });
    }
}

fn preview(
    doc: &Doc,
    cache: &Mutex<Option<Cached>>,
    edit: EditState,
    size: (u32, u32),
    swap: bool,
) -> crate::Result<Arc<Frame>> {
    let original = doc.get()?;
    let geometry = edit.geometry();
    let mut raster = {
        let mut cache = cache.lock().unwrap();
        match &*cache {
            Some((g, s, raster)) if (*g, *s) == (geometry, size) => raster.clone(),
            _ => {
                let raster = geometry.render(&original.raster, Some(size))?;
                *cache = Some((geometry, size, raster.clone()));
                raster
            }
        }
    };
    edit.tone(&mut raster);
    raster.convert_srgb()?;
    let image = image::RgbaImage::from_raw(raster.width, raster.height, raster.rgba)
        .ok_or("invalid preview size")?;
    Pixels::raster(image::DynamicImage::ImageRgba8(image))?.freeze(swap)
}

/// Encodes the edited image, compressing the source when nothing changes its pixels or format.
/// Returns `None` when compressing would not shrink it.
fn encode_with(original: &Original, (edit, export): Key) -> crate::Result<Option<Vec<u8>>> {
    if export.format.is_none() && edit.is_identity() {
        let options = CompressOptions {
            lossy: export.lossy,
            quality: export.quality,
            keep_metadata: export.keep,
        };
        return encode::compress(&original.bytes, &options);
    }
    let format = export.format.or(original.format).unwrap_or(Format::Png);
    let options = ConvertOptions {
        format,
        quality: export.quality,
        lossless: !export.lossy && format != Format::Jpeg,
        resize: None,
        background: [255; 3],
        keep_metadata: export.keep,
    };
    encode::convert(edit.apply(&original.raster)?, &options).map(Some)
}

struct Saved {
    path: PathBuf,
    before: usize,
    after: usize,
}

/// Writes next to the source, or to the pictures folder for stdin, without replacing files,
/// or replaces the source when `overwrite` is set.
fn save(
    doc: &Doc,
    key: Key,
    encoded: Option<Option<Arc<Vec<u8>>>>,
    overwrite: bool,
) -> crate::Result<Option<Saved>> {
    let original = doc.get()?;
    let encoded = match encoded {
        Some(encoded) => encoded,
        None => encode_with(original, key)?.map(Arc::new),
    };
    let Some(bytes) = encoded else {
        return Ok(None);
    };
    let source = doc.source.path();
    let path = if overwrite {
        // Replacing the resolved file keeps a symlink pointing at the new contents.
        let target = std::fs::canonicalize(source.ok_or("there is no file to overwrite")?)?;
        write(Some(&target), &target, &bytes, true)?;
        target
    } else {
        let stamp = timestamp();
        let mut number = 1;
        loop {
            let path = sibling(source, original, key, &stamp, number)?;
            // Another writer or a dangling symlink can still take the name before the rename.
            if !path.try_exists()? {
                match write(source, &path, &bytes, false) {
                    Ok(()) => break path,
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) => {}
                    Err(e) => return Err(e),
                }
            }
            number += 1;
        }
    };
    Ok(Some(Saved {
        path,
        before: original.bytes.len(),
        after: bytes.len(),
    }))
}

fn sibling(
    source: Option<&Path>,
    original: &Original,
    (edit, export): Key,
    stamp: &str,
    number: u32,
) -> crate::Result<PathBuf> {
    let format = export.format.or(original.format).unwrap_or(Format::Png);
    let number = if number > 1 {
        format!("-{number}")
    } else {
        String::new()
    };
    let Some(source) = source else {
        let folder = pictures().ok_or("cannot find a folder to save in")?;
        return Ok(folder.join(format!("perch-{stamp}{number}.{}", format.extension())));
    };
    let extension = source.extension().and_then(|e| e.to_str());
    let same = extension.and_then(Format::parse) == Some(format);
    let suffix = match (same, export.format.is_none() && edit.is_identity()) {
        (false, _) => "",
        (true, true) => "-min",
        (true, false) => "-edited",
    };
    let mut name = source
        .file_stem()
        .ok_or("invalid image path")?
        .to_os_string();
    name.push(format!("{suffix}{number}."));
    name.push(extension.filter(|_| same).unwrap_or(format.extension()));
    Ok(source.with_file_name(name))
}

fn pictures() -> Option<PathBuf> {
    let folder = std::env::var_os("XDG_PICTURES_DIR").map(PathBuf::from);
    if let Some(folder) = folder.filter(|f| f.is_dir()) {
        return Some(folder);
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let pictures = home.join("Pictures");
    Some(if pictures.is_dir() { pictures } else { home })
}

/// The local time as `YYYYMMDD-HHMMSS`.
fn timestamp() -> String {
    // localtime_r only writes the zeroed struct it is given.
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Shortens the home folder to `~`.
fn shown(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// PNG bytes for the clipboard, delivered to paste requests once encoded.
#[derive(Default)]
pub(super) struct Clip(Mutex<Option<Option<Arc<Vec<u8>>>>>, Condvar);
impl Clip {
    fn set(&self, bytes: Option<Arc<Vec<u8>>>) {
        *self.0.lock().unwrap() = Some(bytes);
        self.1.notify_all();
    }
    /// Waits for encoding to finish, returning `None` when it failed.
    pub(super) fn wait(&self) -> Option<Arc<Vec<u8>>> {
        let mut bytes = self.0.lock().unwrap();
        while bytes.is_none() {
            bytes = self.1.wait(bytes).unwrap();
        }
        bytes.clone().flatten()
    }
}
/// Releases paste requests without data when encoding ends without a result,
/// such as after a panic or when the task never runs.
struct ClipGuard(Arc<Clip>);
impl Drop for ClipGuard {
    fn drop(&mut self) {
        if self.0.0.lock().unwrap().is_none() {
            self.0.set(None);
        }
    }
}
enum Snapshot {
    Edited(Arc<Doc>, EditState),
    Source(Arc<Source>),
}
fn clipboard_png(snapshot: Snapshot) -> crate::Result<Vec<u8>> {
    let mut raster = match snapshot {
        Snapshot::Edited(doc, edit) => edit.apply(&doc.get()?.raster)?,
        Snapshot::Source(source) => Raster::load(&source.read()?, source.path())?,
    };
    // Paste targets often ignore profiles, so the clipboard holds plain sRGB without
    // private metadata, even for PNG sources.
    raster.convert_srgb()?;
    (raster.exif, raster.xmp) = (None, None);
    encode::fast_png(&raster)
}

impl App {
    pub(super) fn editing(&self, id: u64) -> bool {
        self.pins.get(&id).is_some_and(Pin::editing)
    }
    /// Enters or leaves edit mode.
    pub(super) fn toggle_edit(&mut self, id: u64) {
        let serial = self.id();
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        if pin.work == Work::Loading || pin.request.is_some() {
            return;
        }
        let min = hud::min_size(&mut self.labels, pin.scale as f32);
        match pin.editor.as_deref_mut() {
            Some(editor) if editor.active => return pin.exit_edit(),
            Some(editor) => (editor.active, editor.min) = (true, min),
            None => {
                pin.editor = Some(Box::new(Editor {
                    serial,
                    doc: Arc::new(Doc {
                        source: pin.source.clone(),
                        cell: OnceLock::new(),
                    }),
                    size: pin.source.size,
                    history: History::default(),
                    active: true,
                    tool: None,
                    crop: [0.0; 4],
                    drag: None,
                    hover: None,
                    export: Export::default(),
                    estimate: None,
                    estimating: false,
                    previewing: false,
                    cache: Arc::default(),
                    restore: (pin.zoom, pin.size, pin.inspect, pin.center),
                    min,
                }));
                pin.playing = false;
            }
        }
        pin.fit_window();
        pin.title();
    }
    /// Handles a key in edit mode, returning whether it was an editing key.
    pub(super) fn edit_key(&mut self, id: u64, key: u32, shift: bool) -> bool {
        let Some(pin) = self.pins.get_mut(&id) else {
            return false;
        };
        let cropping = pin
            .editor
            .as_ref()
            .is_some_and(|e| e.tool == Some(Tool::Crop));
        let control = match key {
            keysyms::KEY_c => Control::Tool(Tool::Crop),
            keysyms::KEY_r if !shift => Control::Rotate,
            keysyms::KEY_r => {
                pin.change(|s, size| s.rotate(size, false));
                return true;
            }
            keysyms::KEY_h => Control::FlipH,
            keysyms::KEY_v => Control::FlipV,
            keysyms::KEY_t => Control::Tool(Tool::Tone),
            keysyms::KEY_z => Control::Tool(Tool::Resize),
            keysyms::KEY_x => Control::Tool(Tool::Export),
            keysyms::KEY_Return | keysyms::KEY_KP_Enter if cropping => {
                pin.end_crop(true);
                return true;
            }
            keysyms::KEY_Escape => {
                let editor = pin.editor.as_deref_mut().unwrap();
                if cropping {
                    pin.end_crop(false);
                } else if editor.tool.is_some() {
                    editor.tool = None;
                    pin.redraw = true;
                } else {
                    pin.exit_edit();
                }
                return true;
            }
            _ => return false,
        };
        pin.control(control);
        true
    }
    pub(super) fn control(&mut self, id: u64, control: Control) {
        match control {
            Control::Save => self.save(id, false),
            Control::Overwrite => self.save(id, true),
            _ => {
                if let Some(pin) = self.pins.get_mut(&id) {
                    pin.control(control);
                }
            }
        }
    }
    pub(super) fn undo(&mut self, id: u64, redo: bool) {
        if let Some(pin) = self.pins.get_mut(&id).filter(|p| p.editing()) {
            pin.history(if redo { History::redo } else { History::undo });
        }
    }
    fn widgets(&mut self, id: u64) -> Option<Vec<hud::Widget>> {
        let pin = self.pins.get(&id)?;
        let editor = pin.editor.as_deref().filter(|e| e.active)?;
        let window = (pin.size.0 as f32, pin.size.1 as f32);
        Some(hud::layout(
            editor,
            &mut self.labels,
            pin.scale as f32,
            window,
        ))
    }
    /// Handles a left press in edit mode, returning whether the HUD or crop box took it.
    pub(super) fn edit_press(&mut self, id: u64, position: (f64, f64)) -> bool {
        let Some(widgets) = self.widgets(id) else {
            return false;
        };
        let point = (position.0 as f32, position.1 as f32);
        let hit = hud::hit(&widgets, point);
        let pin = self.pins.get_mut(&id).unwrap();
        match hit {
            hud::Hit::Control(control @ (Control::Tone(_) | Control::Quality), rect) => {
                let editor = pin.editor.as_deref_mut().unwrap();
                editor.drag = Some(Drag::Slider(control, rect, true));
                pin.slide(point.0);
            }
            hud::Hit::Control(control, _) => self.control(id, control),
            hud::Hit::Panel => {}
            hud::Hit::None => {
                let crop = pin.crop_rect();
                let image = pin.image_point(position);
                let editor = pin.editor.as_deref_mut().unwrap();
                let Some(crop) = crop else {
                    return false;
                };
                let handle = hud::crop_handle(crop, point);
                editor.drag = Some(Drag::Crop(handle, image, editor.crop));
                if handle.is_none() {
                    editor.crop = [image.0, image.1, image.0, image.1];
                }
                pin.redraw = true;
            }
        }
        true
    }
    pub(super) fn edit_motion(&mut self, id: u64, position: (f64, f64)) {
        let Some(widgets) = self.widgets(id) else {
            return;
        };
        let point = (position.0 as f32, position.1 as f32);
        let pin = self.pins.get_mut(&id).unwrap();
        let image = pin.image_point(position);
        let editor = pin.editor.as_deref_mut().unwrap();
        let hover = match hud::hit(&widgets, point) {
            hud::Hit::Control(control, _) => Some(control),
            _ => None,
        };
        if editor.hover != hover {
            editor.hover = hover;
            pin.redraw = true;
        }
        let editor = pin.editor.as_deref_mut().unwrap();
        match editor.drag {
            Some(Drag::Slider(..)) => pin.slide(point.0),
            Some(Drag::Crop(handle, from, start)) => {
                editor.crop = drag_crop(handle, from, start, image, editor.image_size());
                pin.redraw = true;
            }
            None => {}
        }
    }
    pub(super) fn edit_release(&mut self, id: u64) {
        let Some(editor) = self.pins.get_mut(&id).and_then(|p| p.editor.as_deref_mut()) else {
            return;
        };
        editor.end_drag();
        self.pins.get_mut(&id).unwrap().redraw = true;
    }
    pub(super) fn edit_leave(&mut self, id: u64) {
        if let Some(pin) = self.pins.get_mut(&id)
            && let Some(editor) = pin.editor.as_deref_mut()
            && editor.hover.take().is_some()
        {
            pin.redraw = true;
        }
    }
    /// Cancels cropping or leaves edit mode.
    pub(super) fn edit_back(&mut self, id: u64) {
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        if pin
            .editor
            .as_ref()
            .is_some_and(|e| e.tool == Some(Tool::Crop))
        {
            pin.end_crop(false);
        } else {
            pin.exit_edit();
        }
    }
    fn previewed(
        &mut self,
        id: u64,
        serial: u64,
        result: Result<Arc<Frame>, String>,
        qh: &QueueHandle<App>,
    ) {
        let Some(pin) = self.pins.get_mut(&id) else {
            return;
        };
        let Some(editor) = pin.editor.as_deref_mut().filter(|e| e.serial == serial) else {
            return;
        };
        editor.previewing = false;
        match result {
            // Frames for an outdated size wait for the preview already requested.
            Ok(frame) => {
                let (_, _, w, h) = pin.content_px();
                if (frame.width, frame.height) == (w, h) || pin.base.is_none() {
                    self.present(id, frame, None, false, qh);
                }
            }
            // The edits stay so the change that failed can be undone.
            Err(error) => self.toast(id, error),
        }
    }
    pub(super) fn toast(&mut self, id: u64, text: String) {
        if let Some(pin) = self.pins.get_mut(&id) {
            pin.toast = Some((text, Instant::now() + Duration::from_secs(3)));
            pin.redraw = true;
        }
    }
    /// Saves next to the source, or replaces it when `overwrite` is set.
    pub(super) fn save(&mut self, id: u64, overwrite: bool) {
        let Some(editor) = self.pins.get(&id).and_then(|p| p.editor.as_deref()) else {
            return;
        };
        if overwrite && editor.can_overwrite() != Some(true) {
            return;
        }
        let (doc, key, serial) = (editor.doc.clone(), editor.key(), editor.serial);
        // The estimate of the same export is the file to write.
        let encoded = (editor.estimate.as_ref())
            .filter(|(k, _)| *k == key)
            .and_then(|(_, r)| r.as_ref().ok())
            .map(|e| e.after.clone());
        let spawned = self.worker.as_ref().unwrap().spawn(move || {
            let result = save(&doc, key, encoded, overwrite).map_err(|e| e.to_string());
            Box::new(move |app: &mut App, _: &QueueHandle<App>| {
                // Reload a replaced source only when no edits were made since saving.
                let reload = overwrite
                    && matches!(result, Ok(Some(_)))
                    && (app.pins.get(&id).and_then(|p| p.editor.as_deref()))
                        .is_some_and(|e| e.serial == serial && e.history.current == key.0);
                let text = match result {
                    Ok(None) => "Already optimal".into(),
                    Ok(Some(saved)) => {
                        let mut path = shown(&saved.path);
                        if path.chars().count() > 48 {
                            path = saved.path.file_name().unwrap().to_string_lossy().into();
                        }
                        format!(
                            "Saved {path} {} → {}",
                            size(saved.before),
                            size(saved.after)
                        )
                    }
                    Err(error) => error,
                };
                app.toast(id, text);
                if reload {
                    app.reload(id);
                }
            })
        });
        if !spawned {
            self.toast(id, "cannot start saving".into());
        }
    }
    /// Copies the shown image as PNG to the clipboard of `seat`.
    pub(super) fn copy(&mut self, id: u64, seat: u32, qh: &QueueHandle<App>) {
        let (Some(pin), Some(seat)) = (self.pins.get(&id), self.seats.get(&seat)) else {
            return;
        };
        let (Some(manager), Some(device)) = (&self.data_manager, &seat.data_device) else {
            self.toast(id, "The clipboard is unavailable".into());
            return;
        };
        let snapshot = match pin.editor.as_deref() {
            Some(e) if !e.history.current.is_identity() => {
                Snapshot::Edited(e.doc.clone(), e.history.current)
            }
            _ => Snapshot::Source(pin.source.clone()),
        };
        let clip = Arc::new(Clip::default());
        let source = manager.create_data_source(qh, clip.clone());
        source.offer("image/png".into());
        device.set_selection(Some(&source), seat.serial);
        if let Some(old) = self.clipboard.replace(source) {
            old.destroy();
        }
        let encoded = ClipGuard(clip.clone());
        let spawned = self.worker.as_ref().unwrap().spawn(move || {
            let result = clipboard_png(snapshot).map(Arc::new);
            encoded.0.set(result.as_ref().ok().cloned());
            let result = result.map_err(|e| e.to_string());
            Box::new(move |app: &mut App, _: &QueueHandle<App>| {
                let text = result.map_or_else(|e| e, |_| "Copied to clipboard".into());
                app.toast(id, text);
            })
        });
        if !spawned {
            self.toast(id, "cannot start copying".into());
        }
    }
}
