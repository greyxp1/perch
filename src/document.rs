use crate::{
    Result,
    image::{Frame, Pixels, Tiff, limits, oriented},
};
use fast_image_resize::{
    PixelType::U8x4,
    ResizeOptions, Resizer,
    images::{CroppedImageMut, Image, ImageRef},
};
use image::{
    AnimationDecoder, DynamicImage, Frames, ImageDecoder, ImageFormat, ImageReader,
    metadata::{LoopCount, Orientation},
};
use rustix::fs::{self, Mode, OFlags};
use std::{
    fs::File,
    io::{BufReader, Read, Seek},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

type Rect = (u32, u32, u32, u32);

pub const CACHE_BYTES: usize = 64 * 1024 * 1024;
#[derive(Clone)]
pub enum Input {
    Path(PathBuf),
    File(Arc<File>),
}
impl Input {
    pub(crate) fn file(&self) -> Result<File> {
        match self {
            Self::Path(path) => {
                let file = File::from(fs::open(
                    path,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                if !file.metadata()?.is_file() {
                    return Err("expected a regular image file".into());
                }
                Ok(file)
            }
            Self::File(file) => Ok(File::open(crate::fd_path(file.as_ref()))?),
        }
    }
    fn reader(&self) -> Result<BufReader<File>> {
        Ok(BufReader::new(self.file()?))
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(p) => Some(p),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Playback {
    Gif,
    Png,
    WebP,
    Extra(crate::extra::Kind),
}
pub struct Source {
    input: Input,
    pub size: (u32, u32),
    pub playback: Option<Playback>,
    orientation: Orientation,
    data: Data,
}
enum Data {
    Raster {
        frame: Arc<Frame>,
        format: Option<ImageFormat>,
    },
    Tiff {
        levels: Vec<Level>,
        overview: Mutex<Option<Arc<Frame>>>,
    },
}
struct Level {
    offset: u64,
    width: u32,
    height: u32,
}
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub width: u32,
    pub height: u32,
    pub left: f64,
    pub top: f64,
    pub span_width: f64,
    pub span_height: f64,
}
impl Source {
    pub fn load(
        input: Input,
        swap: bool,
        limit: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Arc<Self>> {
        let mut reader = input.reader()?;
        let format = ImageReader::new(&mut reader)
            .with_guessed_format()?
            .format();
        if (format.is_none() || format == Some(ImageFormat::Avif))
            && let Some(kind) = crate::extra::kind(&mut reader)?
        {
            let (pixels, animated) = crate::extra::preview(input.file()?, kind, limit)?;
            let playback = animated.then_some(Playback::Extra(kind));
            return Self::pixels(input, pixels, playback, swap, format);
        }
        if matches!(format, None | Some(ImageFormat::Tiff))
            && let Some(pixels) = crate::native::load(input.file()?, limit, cancelled)?
        {
            return Self::pixels(input, pixels, None, swap, format);
        }
        let format = format.or_else(|| input.path().and_then(|p| ImageFormat::from_path(p).ok()));
        if format == Some(ImageFormat::Tiff)
            && let Ok(mut native) = Tiff::open(input.file()?)
            && native.readable()
        {
            let raw_size = native.dimensions()?;
            if !native.tiled()
                && u64::from(raw_size.0) * u64::from(raw_size.1) <= crate::RENDER_BYTES as u64 / 8
            {
                if u64::from(raw_size.0) * u64::from(raw_size.1) > limit as u64 / 4 {
                    return Err("image exceeds the decoded memory budget".into());
                }
                let orientation =
                    Orientation::from_exif(native.orientation().try_into().unwrap_or(1))
                        .unwrap_or(Orientation::NoTransforms);
                let pixels = Pixels::tiff(native, raw_size)
                    .and_then(|p| p.orient(orientation))
                    .or_else(|_| {
                        Pixels::load(input.reader()?, format, input.path(), limit, cancelled)
                    })?;
                return Self::pixels(input, pixels, None, swap, format);
            }
            let orientation = Orientation::from_exif(native.orientation().try_into()?)
                .ok_or("invalid TIFF orientation")?;
            let mut levels = Vec::new();
            let mut pending = vec![(native.offset(), false)];
            let mut visited = std::collections::HashSet::from([native.offset()]);
            while let Some((offset, subifd)) = pending.pop() {
                if cancelled() {
                    return Err("image loading cancelled".into());
                }
                native.seek(offset)?;
                let (w, h) = native.dimensions()?;
                if (levels.is_empty() || subifd || native.reduced())
                    && native.readable()
                    && native.orientation() == u16::from(orientation.to_exif())
                    && w <= raw_size.0
                    && h <= raw_size.1
                    && (f64::from(w) / f64::from(h) - f64::from(raw_size.0) / f64::from(raw_size.1))
                        .abs()
                        < 0.01
                {
                    levels.push(Level {
                        offset,
                        width: w,
                        height: h,
                    });
                }
                let children = native.children();
                let next = native.next().then(|| (native.offset(), false));
                for child in children
                    .into_iter()
                    .map(|offset| (offset, true))
                    .chain(next)
                {
                    if visited.insert(child.0) {
                        if visited.len() > limit / 64 {
                            return Err("TIFF directories exceed the memory budget".into());
                        }
                        pending.push(child);
                    }
                }
            }
            levels.sort_by_key(|l| std::cmp::Reverse(u64::from(l.width) * u64::from(l.height)));
            return Ok(Arc::new(Self {
                input,
                size: oriented(raw_size, orientation),
                playback: None,
                orientation,
                data: Data::Tiff {
                    levels,
                    overview: Mutex::new(None),
                },
            }));
        }
        let dimensions = |(w, h): (u32, u32)| -> Result<()> {
            if u64::from(w) * u64::from(h) > limit as u64 / 4 {
                return Err("image exceeds the decoded memory budget".into());
            }
            Ok(())
        };
        let animated = match format {
            Some(ImageFormat::Png) => {
                let decoder =
                    image::codecs::png::PngDecoder::with_limits(&mut reader, limits(limit))?;
                dimensions(decoder.dimensions())?;
                decoder.is_apng()?.then_some(Playback::Png)
            }
            Some(ImageFormat::WebP) => {
                let mut header = [0; 64];
                let length = reader.read(&mut header)?;
                let (size, animated) = match crate::image::webp_info(&header[..length]) {
                    Ok(info) => info,
                    Err(_) => {
                        reader.rewind()?;
                        let decoder = image::codecs::webp::WebPDecoder::new(&mut reader)?;
                        (decoder.dimensions(), decoder.has_animation())
                    }
                };
                dimensions(size)?;
                animated.then_some(Playback::WebP)
            }
            Some(ImageFormat::Gif) => Some(Playback::Gif),
            _ => None,
        };
        reader.rewind()?;
        let pixels = Pixels::load(reader, format, input.path(), limit, cancelled)?;
        Self::pixels(input, pixels, animated, swap, format)
    }
    fn pixels(
        input: Input,
        pixels: Pixels,
        playback: Option<Playback>,
        swap: bool,
        format: Option<ImageFormat>,
    ) -> Result<Arc<Self>> {
        let size = pixels.original_size;
        if playback.is_none()
            && size == (pixels.width as u32, pixels.height as u32)
            && let Input::File(file) = &input
        {
            // A complete still frame never needs its private encoded spool again.
            file.set_len(0)?;
        }
        let orientation = pixels.orientation;
        let frame = pixels.freeze(swap)?;
        Ok(Arc::new(Self {
            input,
            size,
            playback,
            orientation,
            data: Data::Raster { frame, format },
        }))
    }
    pub fn path(&self) -> Option<&Path> {
        self.input.path()
    }
    pub fn bytes(&self) -> usize {
        match &self.data {
            Data::Raster { frame, .. } => frame.bytes(),
            Data::Tiff { overview, .. } => {
                overview.lock().unwrap().as_ref().map_or(0, |f| f.bytes())
            }
        }
    }
    pub fn render(
        &self,
        view: View,
        animation: Option<&Arc<Frame>>,
        swap: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Arc<Frame>> {
        let (sw, sh) = (f64::from(self.size.0), f64::from(self.size.1));
        let l = view.left.max(0.0);
        let t = view.top.max(0.0);
        let r = (view.left + view.span_width).min(sw);
        let b = (view.top + view.span_height).min(sh);
        let project = |value: f64, origin: f64, span: f64, extent: u32| {
            ((value - origin) / span * f64::from(extent))
                .round()
                .clamp(0.0, f64::from(extent)) as u32
        };
        let x = project(l, view.left, view.span_width, view.width);
        let y = project(t, view.top, view.span_height, view.height);
        let right = project(r, view.left, view.span_width, view.width);
        let bottom = project(b, view.top, view.span_height, view.height);
        // Empty or inverted spans project to empty destinations.
        let (w, h) = (right.saturating_sub(x), bottom.saturating_sub(y));
        if w == 0 || h == 0 {
            return Pixels::new(view.width.try_into()?, view.height.try_into()?)?.freeze(false);
        }
        let rect = (l, t, r, b);
        let (frame, crop) = match &self.data {
            Data::Tiff { levels, overview } => {
                self.region(levels, overview, rect, (w, h), swap, cancelled)?
            }
            Data::Raster { frame, format } => {
                let required = (f64::from(view.width) / view.span_width)
                    .max(f64::from(view.height) / view.span_height)
                    .min(1.0);
                let scale = (f64::from(frame.width) / sw).min(f64::from(frame.height) / sh);
                if animation.is_none()
                    && scale + 1e-8 < required
                    && let Some(format) = format
                {
                    self.raster_region(rect, (w, h), swap, cancelled, *format)?
                } else {
                    let frame = animation.unwrap_or(frame).clone();
                    crop(frame, (0, 0, self.size.0, self.size.1), rect)
                }
            }
        };
        render_frame(frame, crop, view, (x, y, w, h))
    }
    fn raster_region(
        &self,
        rect: (f64, f64, f64, f64),
        output: (u32, u32),
        swap: bool,
        cancelled: &dyn Fn() -> bool,
        format: ImageFormat,
    ) -> Result<(Arc<Frame>, [f64; 4])> {
        let even = format == ImageFormat::WebP;
        let (padded, raw) = bounds(rect, output, self.size, self.orientation, even);
        let raw_size = oriented(self.size, self.orientation);
        let span = (raw.2 - raw.0, raw.3 - raw.1);
        let target = if u64::from(span.0) * u64::from(span.1) > crate::RENDER_BYTES as u64 / 8 {
            crate::overview::fit(span, output.0.max(output.1))
        } else {
            span
        };
        let reader = self.input.reader()?;
        let pixels = if format == ImageFormat::Png {
            Pixels::png_region(reader, raw_size, raw, target, cancelled)?
        } else {
            Pixels::encoded_region(reader, format, raw_size, raw, target, cancelled)?
        };
        if pixels.original_size != self.size || pixels.orientation != self.orientation {
            return Err("image changed while open".into());
        }
        Ok(crop(pixels.freeze(swap)?, padded, rect))
    }
    fn region(
        &self,
        levels: &[Level],
        overview: &Mutex<Option<Arc<Frame>>>,
        rect: (f64, f64, f64, f64),
        output: (u32, u32),
        swap: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(Arc<Frame>, [f64; 4])> {
        let mut level = &levels[0];
        for candidate in levels {
            let (w, h) = oriented((candidate.width, candidate.height), self.orientation);
            if (rect.2 - rect.0) * f64::from(w) / f64::from(self.size.0) >= f64::from(output.0)
                && (rect.3 - rect.1) * f64::from(h) / f64::from(self.size.1) >= f64::from(output.1)
            {
                level = candidate;
            }
        }
        let size = oriented((level.width, level.height), self.orientation);
        let sx = f64::from(size.0) / f64::from(self.size.0);
        let sy = f64::from(size.1) / f64::from(self.size.1);
        let scaled = (rect.0 * sx, rect.1 * sy, rect.2 * sx, rect.3 * sy);
        let (padded, raw) = bounds(scaled, output, size, self.orientation, false);
        let mut decoder = Tiff::open(self.input.file()?)?;
        decoder.seek(level.offset)?;
        if decoder.dimensions()? != (level.width, level.height)
            || !decoder.readable()
            || decoder.orientation() != u16::from(self.orientation.to_exif())
        {
            return Err("TIFF changed while open".into());
        }
        let full = rect == (0.0, 0.0, f64::from(self.size.0), f64::from(self.size.1));
        let reduce = u64::from(padded.2 - padded.0) * u64::from(padded.3 - padded.1)
            > crate::RENDER_BYTES as u64 / 8;
        let mut cache = (reduce && full).then(|| overview.lock().unwrap());
        if let Some(frame) = cache.as_deref().and_then(Option::as_ref)
            && frame.width as u32 >= output.0
            && frame.height as u32 >= output.1
        {
            return Ok((
                frame.clone(),
                [0.0, 0.0, f64::from(frame.width), f64::from(frame.height)],
            ));
        }
        let out = if reduce {
            decoder.overview(raw, oriented(output, self.orientation), cancelled)?
        } else {
            decoder.region(raw, cancelled)?
        };
        let frame = out.orient(self.orientation)?.freeze(swap)?;
        if let Some(cache) = cache.as_deref_mut() {
            *cache = Some(frame.clone());
        }
        Ok(crop(frame, padded, scaled))
    }
}
pub fn neighbors(path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return vec![path.to_path_buf()];
    };
    let mut paths = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && (ImageFormat::from_path(p).is_ok()
                    || p.extension().is_some_and(|e| {
                        e.eq_ignore_ascii_case("svg")
                            || e.eq_ignore_ascii_case("svgz")
                            || image::hooks::decoding_hook_registered(e)
                            || crate::native::extension(&e.to_string_lossy().to_ascii_lowercase())
                    }))
        })
        .collect::<Vec<_>>();
    if !paths.iter().any(|p| p == path) {
        paths.push(path.to_path_buf());
    }
    paths.sort();
    paths
}

pub struct Animation {
    pub source: Arc<Source>,
    playback: Playback,
    frames: Frames<'static>,
    color: Option<crate::color::Transform>,
    remaining: Option<u32>,
    pub current: Option<Arc<Frame>>,
    ended: bool,
}
impl Animation {
    pub fn new(source: Arc<Source>) -> Result<Self> {
        let playback = source.playback.ok_or("image is not animated")?;
        let (frames, count) = frames(&source.input, playback)?;
        let format = match playback {
            Playback::Png => Some(ImageFormat::Png),
            Playback::WebP => Some(ImageFormat::WebP),
            _ => None,
        };
        let color = if format.is_some() {
            let mut decoder = crate::image::decoder(source.input.reader()?, format, usize::MAX)?;
            crate::color::Transform::new(decoder.icc_profile()?.as_deref())?
        } else {
            None
        };
        let remaining = match count {
            LoopCount::Infinite => None,
            LoopCount::Finite(n) => Some(n.get()),
        };
        Ok(Self {
            source,
            playback,
            frames,
            color,
            remaining,
            current: None,
            ended: false,
        })
    }
    pub fn next(&mut self, swap: bool) -> Result<Option<Duration>> {
        if self.ended {
            return Ok(None);
        }
        let mut next = self.frames.next();
        if next.is_none() {
            if let Some(n) = &mut self.remaining {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    self.ended = true;
                    return Ok(None);
                }
            }
            self.frames = frames(&self.source.input, self.playback)?.0;
            next = self.frames.next();
        }
        let Some(frame) = next else {
            self.ended = true;
            return Ok(None);
        };
        let frame = frame?;
        let (num, denom) = frame.delay().numer_denom_ms();
        let mut rgba = frame.into_buffer();
        let width = rgba.width() as usize;
        crate::color::apply_rows(self.color.as_mut(), &mut rgba, width, false)?;
        let mut image = DynamicImage::ImageRgba8(rgba);
        image.apply_orientation(self.source.orientation);
        self.current = Some(Pixels::raster(image)?.freeze(swap)?);
        Ok(Some(Duration::from_secs_f64(
            (f64::from(num) / f64::from(denom) / 1000.0).max(0.001),
        )))
    }
}

// Decoders reject replaced files whose contents no longer match the detected format.
fn frames(input: &Input, playback: Playback) -> Result<(Frames<'static>, LoopCount)> {
    fn split<'a>(decoder: impl AnimationDecoder<'a>) -> (Frames<'a>, LoopCount) {
        let count = decoder.loop_count();
        (decoder.into_frames(), count)
    }
    let reader = input.reader()?;
    Ok(match playback {
        Playback::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(reader)?;
            decoder.set_limits(limits(usize::MAX))?;
            split(decoder)
        }
        Playback::Png => {
            split(image::codecs::png::PngDecoder::with_limits(reader, limits(usize::MAX))?.apng()?)
        }
        Playback::WebP => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(reader)?;
            decoder.set_limits(limits(usize::MAX))?;
            split(decoder)
        }
        Playback::Extra(kind) => crate::extra::frames(reader.into_inner(), kind)?,
    })
}

fn render_frame(
    frame: Arc<Frame>,
    mut crop: [f64; 4],
    view: View,
    destination: (u32, u32, u32, u32),
) -> Result<Arc<Frame>> {
    let (x, y, w, h) = destination;
    crop[0] = crop[0].clamp(0.0, f64::from(frame.width));
    crop[1] = crop[1].clamp(0.0, f64::from(frame.height));
    crop[2] = crop[2].min(f64::from(frame.width) - crop[0]);
    crop[3] = crop[3].min(f64::from(frame.height) - crop[1]);
    if x == 0
        && y == 0
        && w == view.width
        && h == view.height
        && crop == [0.0, 0.0, f64::from(frame.width), f64::from(frame.height)]
        && w == frame.width as u32
        && h == frame.height as u32
    {
        return Ok(frame);
    }
    let mut out = Pixels::new(view.width.try_into()?, view.height.try_into()?)?;
    let data = frame.map()?;
    let source = ImageRef::new(frame.width as u32, frame.height as u32, &data, U8x4)?;
    let options = ResizeOptions::new()
        .crop(crop[0], crop[1], crop[2], crop[3])
        .use_alpha(false);
    let mut target = Image::from_slice_u8(view.width, view.height, &mut out.mapping, U8x4)?;
    Resizer::new().resize(
        &source,
        &mut CroppedImageMut::new(&mut target, x, y, w, h)?,
        &options,
    )?;
    // Negative filter lobes must not produce invalid premultiplied colors.
    for p in out.mapping.as_chunks_mut::<4>().0 {
        for c in 0..3 {
            p[c] = p[c].min(p[3]);
        }
    }
    out.freeze(false)
}

/// Projects `rect` into a frame decoded from the integer `bounds` of the same canvas.
fn crop(
    frame: Arc<Frame>,
    (left, top, right, bottom): Rect,
    (l, t, r, b): (f64, f64, f64, f64),
) -> (Arc<Frame>, [f64; 4]) {
    let sx = f64::from(frame.width) / f64::from(right - left);
    let sy = f64::from(frame.height) / f64::from(bottom - top);
    let crop = [
        (l - f64::from(left)) * sx,
        (t - f64::from(top)) * sy,
        (r - l) * sx,
        (b - t) * sy,
    ];
    (frame, crop)
}

/// Pads `rect` for the resampling filter within an oriented canvas of `size`.
/// Returns the padded canvas bounds and the matching raw decoder bounds.
fn bounds(
    (l, t, r, b): (f64, f64, f64, f64),
    (w, h): (u32, u32),
    size: (u32, u32),
    orientation: Orientation,
    even: bool,
) -> (Rect, Rect) {
    let halo = (3.0 * ((r - l) / f64::from(w)).max((b - t) / f64::from(h))).ceil() + 1.0;
    let padded = (
        (l - halo).floor().max(0.0) as u32,
        (t - halo).floor().max(0.0) as u32,
        (r + halo).ceil().min(f64::from(size.0)) as u32,
        (b + halo).ceil().min(f64::from(size.1)) as u32,
    );
    let mut raw = raw_rect(padded, oriented(size, orientation), orientation);
    if !even {
        return (padded, raw);
    }
    // WebP crops start at even raw coordinates.
    raw.0 &= !1;
    raw.1 &= !1;
    let inverse = Orientation::from_exif(match orientation.to_exif() {
        6 => 8,
        8 => 6,
        x => x,
    })
    .unwrap();
    (raw_rect(raw, size, inverse), raw)
}

fn raw_rect(rect: Rect, size: (u32, u32), orientation: Orientation) -> Rect {
    let (left, top, right, bottom) = rect;
    let (w, h) = size;
    match orientation.to_exif() {
        2 => (w - right, top, w - left, bottom),
        3 => (w - right, h - bottom, w - left, h - top),
        4 => (left, h - bottom, right, h - top),
        5 => (top, left, bottom, right),
        6 => (top, h - right, bottom, h - left),
        7 => (w - bottom, h - right, w - top, h - left),
        8 => (w - bottom, left, w - top, right),
        _ => rect,
    }
}
