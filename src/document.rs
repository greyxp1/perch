use crate::{
    Result,
    image::{Frame, Pixels, Tiff, limits, tiff_limits},
};
use fast_image_resize::{
    PixelType::U8x4,
    ResizeOptions, Resizer,
    images::{Image, ImageRef},
};
use image::{
    AnimationDecoder, DynamicImage, Frames, ImageDecoder, ImageFormat, ImageReader,
    metadata::{LoopCount, Orientation},
};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tiff::{
    ColorType,
    decoder::{ChunkType, Decoder, DecodingResult},
    tags::Tag,
};

pub const CACHE_BYTES: usize = 64 * 1024 * 1024;
trait Reader: BufRead + Seek {}
impl<T: BufRead + Seek> Reader for T {}
#[derive(Clone)]
pub enum Input {
    Path(PathBuf),
    File(Arc<File>),
}
impl Input {
    fn file(&self) -> Result<File> {
        match self {
            Self::Path(path) => {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(nix::libc::O_NONBLOCK)
                    .open(path)?;
                if !file.metadata()?.is_file() {
                    return Err("expected a regular image file".into());
                }
                Ok(file)
            }
            Self::File(file) => Ok(File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?),
        }
    }
    fn reader(&self) -> Result<Box<dyn Reader + Send>> {
        Ok(Box::new(BufReader::new(self.file()?)))
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(p) => Some(p),
            _ => None,
        }
    }
}

pub struct Source {
    input: Input,
    pub size: (u32, u32),
    pub animated: bool,
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
            .format()
            .or_else(|| input.path().and_then(|p| ImageFormat::from_path(p).ok()));
        if format == Some(ImageFormat::Tiff) {
            if let Ok(mut native) = Tiff::open(input.file()?)
                && native.readable()
                && (native.tiled() || {
                    let (w, h) = native.dimensions()?;
                    u64::from(w) * u64::from(h) > crate::RENDER_BYTES as u64 / 8
                })
            {
                let raw_size = native.dimensions()?;
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
                        && (f64::from(w) / f64::from(h)
                            - f64::from(raw_size.0) / f64::from(raw_size.1))
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
                let size = if orientation.to_exif() >= 5 {
                    (raw_size.1, raw_size.0)
                } else {
                    raw_size
                };
                return Ok(Arc::new(Self {
                    input,
                    size,
                    animated: false,
                    orientation,
                    data: Data::Tiff {
                        levels,
                        overview: Mutex::new(None),
                    },
                }));
            }
            let mut tiff = Decoder::new(&mut reader)?.with_limits(tiff_limits(limit));
            let size = tiff.dimensions()?;
            if u64::from(size.0) * u64::from(size.1) >= 1_000_000
                && tiff.get_chunk_type() == ChunkType::Strip
                && tiff.colortype()? == ColorType::RGB(8)
                && tiff
                    .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)?
                    .unwrap_or(1)
                    == 1
            {
                if u64::from(size.0) * u64::from(size.1) * 4 > limit as u64 {
                    return Err("image exceeds the decoded memory budget".into());
                }
                let rows = tiff.chunk_dimensions().1.min(size.1);
                let orientation = tiff
                    .find_tag_unsigned::<u16>(Tag::Orientation)?
                    .and_then(|n| Orientation::from_exif(n.try_into().ok()?))
                    .unwrap_or(Orientation::NoTransforms);
                let compression = tiff
                    .find_tag_unsigned::<u16>(Tag::Compression)?
                    .unwrap_or(1);
                drop(tiff);
                drop(reader);
                let pixels = if crate::image::native_tiff(compression) {
                    Pixels::tiff(input.file()?, size).or_else(|_| strips(&input, size, rows))?
                } else {
                    strips(&input, size, rows)?
                }
                .orient(orientation)?;
                return Self::pixels(input, pixels, false, swap, Some(ImageFormat::Tiff));
            }
            drop(tiff);
            reader.rewind()?;
        }
        let dimensions = |(w, h): (u32, u32)| -> Result<()> {
            if u64::from(w) * u64::from(h) * 4 > limit as u64 {
                return Err("image exceeds the decoded memory budget".into());
            }
            Ok(())
        };
        let animated = match format {
            Some(ImageFormat::Png) => {
                let decoder =
                    image::codecs::png::PngDecoder::with_limits(&mut reader, limits(limit))?;
                dimensions(decoder.dimensions())?;
                decoder.is_apng()?
            }
            Some(ImageFormat::WebP) => {
                let mut header = [0; 64];
                let length = reader.read(&mut header)?;
                if let Ok((size, animated)) = crate::image::webp_info(&header[..length]) {
                    dimensions(size)?;
                    animated
                } else {
                    reader.rewind()?;
                    let decoder = image::codecs::webp::WebPDecoder::new(&mut reader)?;
                    dimensions(decoder.dimensions())?;
                    decoder.has_animation()
                }
            }
            Some(format) => {
                let mut reader = ImageReader::with_format(&mut reader, format);
                reader.limits(limits(limit));
                let decoder = reader.into_decoder()?;
                dimensions(decoder.dimensions())?;
                format == ImageFormat::Gif
            }
            None => false,
        };
        reader.rewind()?;
        let pixels = Pixels::load(
            reader,
            input.path().map(Path::to_path_buf),
            limit,
            cancelled,
        )?;
        Self::pixels(input, pixels, animated, swap, format)
    }
    fn pixels(
        input: Input,
        pixels: Pixels,
        animated: bool,
        swap: bool,
        format: Option<ImageFormat>,
    ) -> Result<Arc<Self>> {
        let size = pixels.original_size;
        if !animated
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
            animated,
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
        if r <= l || b <= t {
            return Pixels::new(view.width.try_into()?, view.height.try_into()?)?.freeze(false);
        }
        let x = ((l - view.left) / view.span_width * f64::from(view.width))
            .round()
            .clamp(0.0, f64::from(view.width)) as u32;
        let y = ((t - view.top) / view.span_height * f64::from(view.height))
            .round()
            .clamp(0.0, f64::from(view.height)) as u32;
        let right = ((r - view.left) / view.span_width * f64::from(view.width))
            .round()
            .clamp(f64::from(x), f64::from(view.width)) as u32;
        let bottom = ((b - view.top) / view.span_height * f64::from(view.height))
            .round()
            .clamp(f64::from(y), f64::from(view.height)) as u32;
        let (w, h) = (right - x, bottom - y);
        if w == 0 || h == 0 {
            return Pixels::new(view.width.try_into()?, view.height.try_into()?)?.freeze(false);
        }
        let (frame, crop) = if let Data::Tiff { levels, overview } = &self.data {
            self.region(levels, overview, (l, t, r, b), (w, h), swap, cancelled)?
        } else {
            let frame = if let Some(frame) = animation {
                frame.clone()
            } else {
                let Data::Raster { frame, format } = &self.data else {
                    unreachable!()
                };
                let required = (f64::from(view.width) / view.span_width)
                    .max(f64::from(view.height) / view.span_height)
                    .min(1.0);
                let scale = (f64::from(frame.width) / sw).min(f64::from(frame.height) / sh);
                if scale + 1e-8 < required {
                    return self.raster_region(
                        view,
                        (l, t, r, b),
                        (x, y, w, h),
                        swap,
                        cancelled,
                        format.unwrap_or(ImageFormat::Jpeg),
                    );
                }
                frame.clone()
            };
            let sx = f64::from(frame.width) / sw;
            let sy = f64::from(frame.height) / sh;
            (frame, [l * sx, t * sy, (r - l) * sx, (b - t) * sy])
        };
        render_frame(frame, crop, view, (x, y, w, h))
    }
    fn raster_region(
        &self,
        view: View,
        rect: (f64, f64, f64, f64),
        destination: (u32, u32, u32, u32),
        swap: bool,
        cancelled: &dyn Fn() -> bool,
        format: ImageFormat,
    ) -> Result<Arc<Frame>> {
        let (l, t, r, b) = rect;
        let (_, _, w, h) = destination;
        let halo = (3.0 * ((r - l) / f64::from(w)).max((b - t) / f64::from(h))).ceil() + 1.0;
        let (left, top, right, bottom) = (
            (l - halo).floor().max(0.0) as u32,
            (t - halo).floor().max(0.0) as u32,
            (r + halo).ceil().min(f64::from(self.size.0)) as u32,
            (b + halo).ceil().min(f64::from(self.size.1)) as u32,
        );
        let rotated = self.orientation.to_exif() >= 5;
        let raw_size = if rotated {
            (self.size.1, self.size.0)
        } else {
            self.size
        };
        let mut raw = raw_rect((left, top, right, bottom), raw_size, self.orientation);
        let (left, top, right, bottom) = if format == ImageFormat::WebP {
            raw.0 &= !1;
            raw.1 &= !1;
            let inverse = Orientation::from_exif(match self.orientation.to_exif() {
                6 => 8,
                8 => 6,
                x => x,
            })
            .unwrap();
            raw_rect(raw, self.size, inverse)
        } else {
            (left, top, right, bottom)
        };
        let raw_span = (raw.2 - raw.0, raw.3 - raw.1);
        let target = if (u64::from(raw_span.0) * u64::from(raw_span.1)).saturating_mul(4)
            > crate::RENDER_BYTES as u64 / 2
        {
            crate::overview::fit(raw_span, w.max(h))
        } else {
            raw_span
        };
        let pixels = if format == ImageFormat::Png {
            Pixels::png_region(self.input.reader()?, raw_size, raw, target, cancelled)?
        } else {
            Pixels::encoded_region(
                self.input.reader()?,
                format,
                raw_size,
                raw,
                target,
                cancelled,
            )?
        };
        if pixels.original_size != self.size || pixels.orientation != self.orientation {
            return Err("image changed while open".into());
        }
        let (sx, sy) = (
            f64::from(pixels.width) / f64::from(right - left),
            f64::from(pixels.height) / f64::from(bottom - top),
        );
        render_frame(
            pixels.freeze(swap)?,
            [
                (l - f64::from(left)) * sx,
                (t - f64::from(top)) * sy,
                (r - l) * sx,
                (b - t) * sy,
            ],
            view,
            destination,
        )
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
        let rotated = self.orientation.to_exif() >= 5;
        let logical = |level: &Level| {
            if rotated {
                (level.height, level.width)
            } else {
                (level.width, level.height)
            }
        };
        let mut level = &levels[0];
        for candidate in levels {
            let (w, h) = logical(candidate);
            if (rect.2 - rect.0) * f64::from(w) / f64::from(self.size.0) >= f64::from(output.0)
                && (rect.3 - rect.1) * f64::from(h) / f64::from(self.size.1) >= f64::from(output.1)
            {
                level = candidate;
            }
        }
        let (lw, lh) = logical(level);
        let sx = f64::from(lw) / f64::from(self.size.0);
        let sy = f64::from(lh) / f64::from(self.size.1);
        let l = rect.0 * sx;
        let t = rect.1 * sy;
        let r = rect.2 * sx;
        let b = rect.3 * sy;
        let halo =
            (3.0 * ((r - l) / f64::from(output.0)).max((b - t) / f64::from(output.1))).ceil() + 1.0;
        let left = (l - halo).floor().max(0.0) as u32;
        let top = (t - halo).floor().max(0.0) as u32;
        let right = (r + halo).ceil().min(f64::from(lw)) as u32;
        let bottom = (b + halo).ceil().min(f64::from(lh)) as u32;
        let (width, height) = (right - left, bottom - top);
        let (w, h) = (level.width, level.height);
        let raw = raw_rect((left, top, right, bottom), (w, h), self.orientation);
        let mut decoder = Tiff::open(self.input.file()?)?;
        decoder.seek(level.offset)?;
        if decoder.dimensions()? != (w, h)
            || !decoder.readable()
            || decoder.orientation() != u16::from(self.orientation.to_exif())
        {
            return Err("TIFF changed while open".into());
        }
        let out = if (u64::from(width) * u64::from(height)).saturating_mul(4)
            > crate::RENDER_BYTES as u64 / 2
        {
            let mut cached = overview.lock().unwrap();
            let full = rect == (0.0, 0.0, f64::from(self.size.0), f64::from(self.size.1));
            if full
                && let Some(frame) = cached.as_ref()
                && frame.width as u32 >= output.0
                && frame.height as u32 >= output.1
            {
                return Ok((
                    frame.clone(),
                    [0.0, 0.0, f64::from(frame.width), f64::from(frame.height)],
                ));
            }
            let size = if rotated {
                (output.1, output.0)
            } else {
                output
            };
            let out = decoder
                .overview(raw, size, cancelled)?
                .orient(self.orientation)?;
            let (sx, sy) = (
                f64::from(out.width) / f64::from(width),
                f64::from(out.height) / f64::from(height),
            );
            let frame = out.freeze(swap)?;
            if full {
                *cached = Some(frame.clone());
            }
            return Ok((
                frame,
                [
                    (l - f64::from(left)) * sx,
                    (t - f64::from(top)) * sy,
                    (r - l) * sx,
                    (b - t) * sy,
                ],
            ));
        } else {
            decoder.region(raw, cancelled)?.orient(self.orientation)?
        };
        Ok((
            out.freeze(swap)?,
            [l - f64::from(left), t - f64::from(top), r - l, b - t],
        ))
    }
}
fn strips(input: &Input, size: (u32, u32), rows: u32) -> Result<Pixels> {
    let mut pixels = Pixels::new(size.0.try_into()?, size.1.try_into()?)?;
    if rows == 0 {
        return Err("invalid TIFF strip height".into());
    }
    let stride = size.0 as usize * rows as usize * 4;
    let mut decoder = Decoder::new(input.reader()?)?.with_limits(tiff_limits(usize::MAX));
    if decoder.dimensions()? != size
        || decoder.colortype()? != ColorType::RGB(8)
        || decoder.get_chunk_type() != ChunkType::Strip
        || decoder.chunk_dimensions().1.min(size.1) != rows
    {
        return Err("TIFF changed while open".into());
    }
    for (index, target) in pixels.mapping.chunks_mut(stride).enumerate() {
        let DecodingResult::U8(bytes) = decoder.read_chunk(index as u32)? else {
            return Err("invalid TIFF samples".into());
        };
        if bytes.len() != target.len() / 4 * 3 {
            return Err("invalid TIFF strip size".into());
        }
        crate::image::expand_rgb(&bytes, target);
    }
    Ok(pixels)
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
                        e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz")
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
    frames: Frames<'static>,
    remaining: Option<u32>,
    pub current: Option<Arc<Frame>>,
    ended: bool,
}
impl Animation {
    pub fn new(source: Arc<Source>) -> Result<Self> {
        let (frames, count) = frames(&source)?;
        let remaining = match count {
            LoopCount::Infinite => None,
            LoopCount::Finite(n) => Some(n.get()),
        };
        Ok(Self {
            source,
            frames,
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
            self.frames = frames(&self.source)?.0;
            next = self.frames.next();
        }
        let Some(frame) = next else {
            self.ended = true;
            return Ok(None);
        };
        let frame = frame?;
        let (num, denom) = frame.delay().numer_denom_ms();
        let mut image = DynamicImage::ImageRgba8(frame.into_buffer());
        image.apply_orientation(self.source.orientation);
        self.current = Some(Pixels::raster(image)?.freeze(swap)?);
        Ok(Some(Duration::from_secs_f64(
            (f64::from(num) / f64::from(denom) / 1000.0).max(0.001),
        )))
    }
}
fn frames(source: &Source) -> Result<(Frames<'static>, LoopCount)> {
    let reader = source.input.reader()?;
    let (frames, count) = match ImageReader::new(source.input.reader()?)
        .with_guessed_format()?
        .format()
    {
        Some(ImageFormat::Gif) => {
            let mut d = image::codecs::gif::GifDecoder::new(reader)?;
            d.set_limits(limits(usize::MAX))?;
            let n = d.loop_count();
            (d.into_frames(), n)
        }
        Some(ImageFormat::Png) => {
            let d = image::codecs::png::PngDecoder::with_limits(reader, limits(usize::MAX))?;
            let d = d.apng()?;
            let n = d.loop_count();
            (d.into_frames(), n)
        }
        Some(ImageFormat::WebP) => {
            let mut d = image::codecs::webp::WebPDecoder::new(reader)?;
            d.set_limits(limits(usize::MAX))?;
            let n = d.loop_count();
            (d.into_frames(), n)
        }
        _ => return Err("image is not animated".into()),
    };
    Ok((frames, count))
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
    if x == 0 && y == 0 && w == view.width && h == view.height {
        Resizer::new().resize(
            &source,
            &mut Image::from_slice_u8(w, h, &mut out.mapping, U8x4)?,
            &options,
        )?;
    } else {
        let mut tile = vec![0; (w as usize) * (h as usize) * 4];
        Resizer::new().resize(
            &source,
            &mut Image::from_slice_u8(w, h, &mut tile, U8x4)?,
            &options,
        )?;
        for row in 0..h as usize {
            let start = ((y as usize + row) * view.width as usize + x as usize) * 4;
            out.mapping[start..start + w as usize * 4]
                .copy_from_slice(&tile[row * w as usize * 4..(row + 1) * w as usize * 4]);
        }
    }
    // Negative filter lobes must not produce invalid premultiplied colors.
    for p in out.mapping.as_chunks_mut::<4>().0 {
        for c in 0..3 {
            p[c] = p[c].min(p[3]);
        }
    }
    out.freeze(false)
}

fn raw_rect(
    rect: (u32, u32, u32, u32),
    size: (u32, u32),
    orientation: Orientation,
) -> (u32, u32, u32, u32) {
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
