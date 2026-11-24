//! Draws the editing controls, crop box and status messages over a pin's frame.
use super::{
    Pin,
    editor::{Control, Editor, Lossy, Tool},
};
use crate::{
    Result,
    encode::Format,
    shm::{Frame, Pixels},
};
use resvg::{
    tiny_skia::{
        BlendMode, Color, FillRule, FilterQuality, Paint, Path, PathBuilder, Pixmap, PixmapMut,
        PixmapPaint, PixmapRef, Rect as SkRect, Stroke, Transform,
    },
    usvg::{self, fontdb},
};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

const FONT: f32 = 12.0;
const ROW: f32 = 26.0;
const PAD: f32 = 6.0;
const GAP: f32 = 2.0;
const INSET: f32 = 8.0;
const MARGIN: f32 = 10.0;
const SLIDER: f32 = 140.0;
const HANDLE: f32 = 8.0;
/// Minimum window height in edit mode, which fits the tallest panel above the bar.
const MIN_HEIGHT: f32 = 260.0;
pub(super) const FORMATS: [(Option<Format>, &str); 11] = [
    (None, "Same"),
    (Some(Format::Png), "PNG"),
    (Some(Format::Jpeg), "JPEG"),
    (Some(Format::WebP), "WebP"),
    (Some(Format::Avif), "AVIF"),
    (Some(Format::Jxl), "JXL"),
    (Some(Format::Tiff), "TIFF"),
    (Some(Format::Bmp), "BMP"),
    (Some(Format::Gif), "GIF"),
    (Some(Format::Ico), "ICO"),
    (Some(Format::Qoi), "QOI"),
];
const TONES: [&str; 3] = ["Brightness", "Contrast", "Saturation"];

/// A rectangle `(x, y, width, height)` in logical window pixels.
pub(super) type Rect = (f32, f32, f32, f32);
fn contains((x, y, w, h): Rect, (px, py): (f32, f32)) -> bool {
    px >= x && py >= y && px < x + w && py < y + h
}

/// Rendered text, cached by content and pixel size.
#[derive(Default)]
pub(super) struct Labels(HashMap<(String, u32), Option<Pixmap>>);
impl Labels {
    fn get(&mut self, text: &str, scale: f32) -> Option<&Pixmap> {
        let px = FONT * scale;
        let key = (text.to_owned(), px.to_bits());
        if self.0.len() > 512 && !self.0.contains_key(&key) {
            self.0.clear();
        }
        self.0
            .entry(key)
            .or_insert_with(|| render_text(text, px))
            .as_ref()
    }
    fn width(&mut self, text: &str, scale: f32) -> f32 {
        self.get(text, scale)
            .map_or(0.0, |p| p.width() as f32 / scale)
    }
}

/// The bundled fallback font when there is one, so labels look the same everywhere.
fn fonts() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = fontdb::Database::new();
            if option_env!("PERCH_FALLBACK_FONT").is_none_or(|f| db.load_font_file(f).is_err()) {
                db.load_system_fonts();
            }
            Arc::new(db)
        })
        .clone()
}

fn render_text(text: &str, px: f32) -> Option<Pixmap> {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let width = px * (text.chars().count() as f32 + 2.0);
    let height = (px * 1.5).ceil();
    let svg = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='{width}' height='{height}'>\
         <text x='0' y='{}' font-family='DejaVu Sans, sans-serif' font-size='{px}' \
         fill='#fff' xml:space='preserve'>{escaped}</text></svg>",
        px * 1.1
    );
    let options = usvg::Options {
        fontdb: fonts(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(&svg, &options).ok()?;
    let right = tree.root().abs_bounding_box().right();
    let mut pixmap = Pixmap::new((right.ceil() as u32 + 1).max(1), height as u32)?;
    resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());
    Some(pixmap)
}

enum Look {
    Panel,
    Button {
        label: String,
        on: bool,
        enabled: bool,
    },
    Slider(f32),
    Text(String),
}
pub(super) struct Widget {
    pub(super) rect: Rect,
    pub(super) control: Option<Control>,
    look: Look,
}
struct Item {
    width: f32,
    control: Option<Control>,
    look: Look,
}

fn button(labels: &mut Labels, scale: f32, label: &str, control: Control, on: bool) -> Item {
    toggle(labels, scale, label, control, on, true)
}
fn toggle(
    labels: &mut Labels,
    scale: f32,
    label: &str,
    control: Control,
    on: bool,
    enabled: bool,
) -> Item {
    Item {
        width: labels.width(label, scale) + 2.0 * INSET,
        control: Some(control),
        look: Look::Button {
            label: label.to_owned(),
            on,
            enabled,
        },
    }
}
fn text(labels: &mut Labels, scale: f32, label: String, width: Option<f32>) -> Item {
    Item {
        width: width.unwrap_or_else(|| labels.width(&label, scale) + INSET),
        control: None,
        look: Look::Text(label),
    }
}
fn slider(control: Control, fraction: f32) -> Item {
    Item {
        width: SLIDER,
        control: Some(control),
        look: Look::Slider(fraction),
    }
}

/// Places rows in a panel centered at `center` whose bottom edge is at `bottom`.
fn panel(widgets: &mut Vec<Widget>, rows: Vec<Vec<Item>>, center: f32, bottom: f32) -> f32 {
    let row_width = |row: &[Item]| {
        row.iter().map(|i| i.width).sum::<f32>() + GAP * row.len().saturating_sub(1) as f32
    };
    let width = rows.iter().map(|r| row_width(r)).fold(0.0, f32::max) + 2.0 * PAD;
    let height = rows.len() as f32 * (ROW + GAP) - GAP + 2.0 * PAD;
    let (x, top) = ((center - width / 2.0).round(), bottom - height);
    widgets.push(Widget {
        rect: (x, top, width, height),
        control: None,
        look: Look::Panel,
    });
    for (index, row) in rows.into_iter().enumerate() {
        let y = top + PAD + index as f32 * (ROW + GAP);
        let mut left = x + PAD;
        for item in row {
            widgets.push(Widget {
                rect: (left, y, item.width, ROW),
                control: item.control,
                look: item.look,
            });
            left += item.width + GAP;
        }
    }
    top
}

fn toolbar(editor: &Editor, labels: &mut Labels, scale: f32) -> Vec<Item> {
    let tool = editor.tool;
    let history = &editor.history;
    let mut items = vec![
        button(
            labels,
            scale,
            "Crop",
            Control::Tool(Tool::Crop),
            tool == Some(Tool::Crop),
        ),
        button(labels, scale, "Rotate", Control::Rotate, false),
        button(labels, scale, "Flip H", Control::FlipH, false),
        button(labels, scale, "Flip V", Control::FlipV, false),
        button(
            labels,
            scale,
            "Tone",
            Control::Tool(Tool::Tone),
            tool == Some(Tool::Tone),
        ),
        button(
            labels,
            scale,
            "Resize",
            Control::Tool(Tool::Resize),
            tool == Some(Tool::Resize),
        ),
        button(
            labels,
            scale,
            "Export",
            Control::Tool(Tool::Export),
            tool == Some(Tool::Export),
        ),
    ];
    items.push(toggle(
        labels,
        scale,
        "Undo",
        Control::Undo,
        false,
        history.can_undo(),
    ));
    items.push(toggle(
        labels,
        scale,
        "Redo",
        Control::Redo,
        false,
        history.can_redo(),
    ));
    let edited = !history.current.is_identity();
    items.push(toggle(
        labels,
        scale,
        "Reset",
        Control::Reset,
        false,
        edited,
    ));
    items
}

fn signed(value: i8) -> String {
    if value == 0 {
        "0".into()
    } else {
        format!("{value:+}")
    }
}

fn tone(editor: &Editor, labels: &mut Labels, scale: f32) -> Vec<Vec<Item>> {
    let name = TONES
        .iter()
        .map(|t| labels.width(t, scale))
        .fold(0.0, f32::max)
        + INSET;
    let value = labels.width("-100", scale) + INSET;
    let state = editor.history.current;
    let values = [state.brightness, state.contrast, state.saturation];
    (0..3)
        .map(|i| {
            vec![
                text(labels, scale, TONES[i].into(), Some(name)),
                slider(Control::Tone(i), (f32::from(values[i]) + 100.0) / 200.0),
                text(labels, scale, signed(values[i]), Some(value)),
            ]
        })
        .collect()
}

fn resize(editor: &Editor, labels: &mut Labels, scale: f32) -> Vec<Vec<Item>> {
    let percent = editor.percent();
    let mut row: Vec<_> = [(75, "75%"), (50, "50%"), (25, "25%"), (100, "Original")]
        .into_iter()
        .map(|(p, label)| button(labels, scale, label, Control::Scale(p), percent == Some(p)))
        .collect();
    row.push(button(labels, scale, "−", Control::Step(false), false));
    row.push(button(labels, scale, "+", Control::Step(true), false));
    let (w, h) = editor.history.current.output(editor.size);
    row.push(text(labels, scale, format!("{w}×{h}"), None));
    vec![row]
}

fn export(editor: &Editor, labels: &mut Labels, scale: f32) -> Vec<Vec<Item>> {
    let settings = editor.export;
    let formats = FORMATS
        .iter()
        .map(|&(format, label)| {
            let on = settings.format == format;
            button(labels, scale, label, Control::Format(format), on)
        })
        .collect();
    let mut options = Vec::new();
    let lossy = editor.lossy();
    if lossy == Lossy::Choice {
        options.push(button(
            labels,
            scale,
            "Lossless",
            Control::Lossy(false),
            !settings.lossy,
        ));
        options.push(button(
            labels,
            scale,
            "Lossy",
            Control::Lossy(true),
            settings.lossy,
        ));
    }
    if lossy == Lossy::Always || (lossy == Lossy::Choice && settings.lossy) {
        options.push(text(labels, scale, "Quality".into(), None));
        let fraction = f32::from(settings.quality - 1) / 99.0;
        options.push(slider(Control::Quality, fraction));
        let value = labels.width("100", scale) + INSET;
        options.push(text(
            labels,
            scale,
            settings.quality.to_string(),
            Some(value),
        ));
    }
    options.push(button(
        labels,
        scale,
        "Strip",
        Control::Keep(false),
        !settings.keep,
    ));
    options.push(button(
        labels,
        scale,
        "Keep",
        Control::Keep(true),
        settings.keep,
    ));
    let mut actions = vec![
        text(labels, scale, editor.estimate_text(), None),
        button(labels, scale, "Save", Control::Save, false),
    ];
    if let Some(enabled) = editor.can_overwrite() {
        actions.push(toggle(
            labels,
            scale,
            "Overwrite",
            Control::Overwrite,
            false,
            enabled,
        ));
    }
    vec![formats, options, actions]
}

/// Lays out the HUD of `editor` in a window of logical `size`.
pub(super) fn layout(
    editor: &Editor,
    labels: &mut Labels,
    scale: f32,
    (width, height): (f32, f32),
) -> Vec<Widget> {
    let mut widgets = Vec::new();
    let center = width / 2.0;
    let bar = toolbar(editor, labels, scale);
    let top = panel(&mut widgets, vec![bar], center, height - MARGIN);
    let rows = match editor.tool {
        Some(Tool::Tone) => tone(editor, labels, scale),
        Some(Tool::Resize) => resize(editor, labels, scale),
        Some(Tool::Export) => export(editor, labels, scale),
        Some(Tool::Crop) | None => return widgets,
    };
    panel(&mut widgets, rows, center, top - 6.0);
    widgets
}

/// The smallest logical window that fits the toolbar and the widest panel.
pub(super) fn min_size(labels: &mut Labels, scale: f32) -> (i32, i32) {
    let formats: f32 = FORMATS
        .iter()
        .map(|(_, label)| labels.width(label, scale) + 2.0 * INSET + GAP)
        .sum();
    let bar = [
        "Crop", "Rotate", "Flip H", "Flip V", "Tone", "Resize", "Export", "Undo",
    ]
    .iter()
    .chain(&["Redo", "Reset"])
    .map(|label| labels.width(label, scale) + 2.0 * INSET + GAP)
    .sum::<f32>();
    let width = formats.max(bar) + 2.0 * (PAD + MARGIN);
    (width.ceil() as i32, MIN_HEIGHT as i32)
}

pub(super) enum Hit {
    None,
    Panel,
    Control(Control, Rect),
}
pub(super) fn hit(widgets: &[Widget], point: (f32, f32)) -> Hit {
    let mut hit = Hit::None;
    for widget in widgets.iter().filter(|w| contains(w.rect, point)) {
        let enabled = !matches!(widget.look, Look::Button { enabled: false, .. });
        hit = match widget.control {
            Some(control) if enabled => Hit::Control(control, widget.rect),
            _ => Hit::Panel,
        };
    }
    hit
}

/// Which crop handle `point` grabs: edges as -1 or 1 per axis, `(0, 0)` inside and `None` outside.
pub(super) fn crop_handle(rect: Rect, point: (f32, f32)) -> Option<(i8, i8)> {
    let (x, y, w, h) = rect;
    let edge = |p: f32, start: f32, size: f32| {
        if (p - start).abs() <= HANDLE {
            -1
        } else if (p - start - size).abs() <= HANDLE {
            1
        } else {
            0
        }
    };
    let grown = (x - HANDLE, y - HANDLE, w + 2.0 * HANDLE, h + 2.0 * HANDLE);
    contains(grown, point).then(|| (edge(point.0, x, w), edge(point.1, y, h)))
}

fn color(gray: u8, alpha: u8) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(gray, gray, gray, alpha));
    paint
}
fn sk_rect((x, y, w, h): Rect, scale: f32) -> Option<SkRect> {
    SkRect::from_xywh(x * scale, y * scale, w * scale, h * scale)
}
fn rounded((x, y, w, h): Rect, radius: f32, scale: f32) -> Option<Path> {
    let (x, y, w, h, r) = (x * scale, y * scale, w * scale, h * scale, radius * scale);
    let r = r.min(w / 2.0).min(h / 2.0);
    // Cubic Bézier circle approximation constant.
    let k = r * 0.447_715;
    let mut path = PathBuilder::new();
    path.move_to(x + r, y);
    path.line_to(x + w - r, y);
    path.cubic_to(x + w - k, y, x + w, y + k, x + w, y + r);
    path.line_to(x + w, y + h - r);
    path.cubic_to(x + w, y + h - k, x + w - k, y + h, x + w - r, y + h);
    path.line_to(x + r, y + h);
    path.cubic_to(x + k, y + h, x, y + h - k, x, y + h - r);
    path.line_to(x, y + r);
    path.cubic_to(x, y + k, x + k, y, x + r, y);
    path.close();
    path.finish()
}
fn fill(canvas: &mut PixmapMut, rect: Rect, radius: f32, paint: &Paint, scale: f32) {
    if let Some(path) = rounded(rect, radius, scale) {
        canvas.fill_path(&path, paint, FillRule::Winding, Transform::identity(), None);
    }
}
fn label(
    canvas: &mut PixmapMut,
    labels: &mut Labels,
    text: &str,
    rect: Rect,
    scale: f32,
    opacity: f32,
    centered: bool,
) {
    let Some(pixmap) = labels.get(text, scale) else {
        return;
    };
    let (x, y, w, h) = rect;
    let left = if centered {
        x * scale + (w * scale - pixmap.width() as f32) / 2.0
    } else {
        (x + INSET / 2.0) * scale
    };
    let top = y * scale + (h * scale - pixmap.height() as f32) / 2.0;
    let paint = PixmapPaint {
        opacity,
        ..Default::default()
    };
    canvas.draw_pixmap(
        left.round() as i32,
        top.round() as i32,
        pixmap.as_ref(),
        &paint,
        Transform::identity(),
        None,
    );
}

fn draw_widgets(
    canvas: &mut PixmapMut,
    widgets: &[Widget],
    hover: Option<Control>,
    labels: &mut Labels,
    scale: f32,
) {
    for widget in widgets {
        let rect = widget.rect;
        match &widget.look {
            Look::Panel => fill(canvas, rect, 8.0, &color(20, 214), scale),
            Look::Button {
                label: text,
                on,
                enabled,
            } => {
                let hovered = *enabled && hover.is_some() && hover == widget.control;
                let background = match (on, hovered) {
                    (true, _) => Some(64),
                    (false, true) => Some(34),
                    _ => None,
                };
                if let Some(alpha) = background {
                    fill(canvas, rect, 5.0, &color(255, alpha), scale);
                }
                let opacity = if *enabled { 1.0 } else { 0.35 };
                label(canvas, labels, text, rect, scale, opacity, true);
            }
            Look::Text(text) => label(canvas, labels, text, rect, scale, 0.75, false),
            Look::Slider(fraction) => {
                let (x, y, w, h) = rect;
                let (left, right) = (x + 6.0, x + w - 6.0);
                let knob = left + (right - left) * fraction.clamp(0.0, 1.0);
                let middle = y + h / 2.0;
                fill(
                    canvas,
                    (left, middle - 1.5, right - left, 3.0),
                    1.5,
                    &color(255, 60),
                    scale,
                );
                fill(
                    canvas,
                    (left, middle - 1.5, knob - left, 3.0),
                    1.5,
                    &color(255, 200),
                    scale,
                );
                let radius = if hover == widget.control { 6.0 } else { 5.0 };
                if let Some(circle) =
                    PathBuilder::from_circle(knob * scale, middle * scale, radius * scale)
                {
                    canvas.fill_path(
                        &circle,
                        &color(255, 255),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }
        }
    }
}

/// Dims outside the crop box and outlines it with handles.
fn draw_crop(canvas: &mut PixmapMut, content: Rect, crop: Rect, scale: f32) {
    let (cx, cy, cw, ch) = content;
    let (x, y, w, h) = crop;
    let dim = color(0, 140);
    let bands = [
        (cx, cy, cw, y - cy),
        (cx, y + h, cw, cy + ch - y - h),
        (cx, y, x - cx, h),
        (x + w, y, cx + cw - x - w, h),
    ];
    for band in bands {
        if let Some(rect) = sk_rect(band, scale) {
            canvas.fill_rect(rect, &dim, Transform::identity(), None);
        }
    }
    if let Some(path) = sk_rect(crop, scale).map(PathBuilder::from_rect) {
        let stroke = Stroke {
            width: scale,
            ..Default::default()
        };
        canvas.stroke_path(
            &path,
            &color(255, 230),
            &stroke,
            Transform::identity(),
            None,
        );
    }
    let size = 7.0;
    for (hx, hy) in [0.0, 0.5, 1.0]
        .iter()
        .flat_map(|&fx| [0.0, 0.5, 1.0].map(|fy| (fx, fy)))
        .filter(|&(fx, fy)| fx != 0.5 || fy != 0.5)
    {
        let rect = (x + w * hx - size / 2.0, y + h * hy - size / 2.0, size, size);
        fill(canvas, rect, 1.5, &color(0, 120), scale);
        let inner = (rect.0 + 1.0, rect.1 + 1.0, size - 2.0, size - 2.0);
        fill(canvas, inner, 1.0, &color(255, 255), scale);
    }
}

fn draw_toast(canvas: &mut PixmapMut, labels: &mut Labels, text: &str, width: f32, scale: f32) {
    let w = labels.width(text, scale) + 2.0 * (INSET + PAD);
    let rect = ((width - w) / 2.0, MARGIN, w, ROW + PAD);
    fill(canvas, rect, 8.0, &color(20, 214), scale);
    label(canvas, labels, text, rect, scale, 1.0, true);
}

/// Draws `base` into the content area of `pin` with its HUD, crop box and toast on top.
pub(super) fn compose(pin: &Pin, base: &Frame, labels: &mut Labels) -> Result<Arc<Frame>> {
    let scale = pin.scale as f32;
    let (bw, bh) = (pin.size.0 * pin.scale, pin.size.1 * pin.scale);
    let mut pixels = Pixels::new(bw, bh)?;
    let mut canvas = PixmapMut::from_bytes(&mut pixels.mapping, bw as u32, bh as u32)
        .ok_or("invalid HUD size")?;
    let (px, py, pw, ph) = pin.content_px();
    let editor = pin.editor.as_deref().filter(|e| e.active);
    if editor.is_some() {
        let backdrop = color(22, 255);
        let bands = [
            (0, 0, bw, py),
            (0, py + ph, bw, bh - py - ph),
            (0, py, px, ph),
            (px + pw, py, bw - px - pw, ph),
        ];
        for (x, y, w, h) in bands {
            if let Some(rect) = SkRect::from_xywh(x as f32, y as f32, w as f32, h as f32) {
                canvas.fill_rect(rect, &backdrop, Transform::identity(), None);
            }
        }
    }
    let map = base.map()?;
    let source = PixmapRef::from_bytes(&map, base.width as u32, base.height as u32)
        .ok_or("invalid frame size")?;
    let exact = (base.width, base.height) == (pw, ph);
    let paint = PixmapPaint {
        blend_mode: BlendMode::Source,
        quality: if exact {
            FilterQuality::Nearest
        } else {
            FilterQuality::Bilinear
        },
        ..Default::default()
    };
    let transform = Transform::from_row(
        pw as f32 / base.width as f32,
        0.0,
        0.0,
        ph as f32 / base.height as f32,
        px as f32,
        py as f32,
    );
    canvas.draw_pixmap(0, 0, source, &paint, transform, None);
    let window = (pin.size.0 as f32, pin.size.1 as f32);
    if let Some(editor) = editor {
        if let Some(crop) = pin.crop_rect() {
            draw_crop(&mut canvas, pin.content_rect(), crop, scale);
        }
        let widgets = layout(editor, labels, scale, window);
        draw_widgets(&mut canvas, &widgets, editor.hover, labels, scale);
    }
    if let Some((text, _)) = &pin.toast {
        draw_toast(&mut canvas, labels, text, window.0, scale);
    }
    pixels.freeze(false)
}
