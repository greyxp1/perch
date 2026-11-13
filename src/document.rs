use crate::{
    Result,
    image::{Frame, Pixels},
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
    fs::OpenOptions,
    io::{BufRead, BufReader, Cursor, Seek},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tiff::{
    ColorType,
    decoder::{ChunkType, Decoder, DecodingResult},
    tags::Tag,
};

pub const CACHE_BYTES: usize = 64 * 1024 * 1024;
const REGION_BYTES: usize = 128 * 1024 * 1024;
trait Reader: BufRead + Seek {}
impl<T: BufRead + Seek> Reader for T {}
struct SharedBytes(Arc<Vec<u8>>);
impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}
#[derive(Clone)]
pub enum Input {
    Path(PathBuf),
    Bytes(Arc<Vec<u8>>),
}
impl Input {
    fn reader(&self) -> Result<Box<dyn Reader + Send>> {
        Ok(match self {
            Self::Path(path) => {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(nix::libc::O_NONBLOCK)
                    .open(path)?;
                if !file.metadata()?.is_file() {
                    return Err("expected a regular image file".into());
                }
                Box::new(BufReader::new(file))
            }
            Self::Bytes(bytes) => Box::new(Cursor::new(SharedBytes(bytes.clone()))),
        })
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
    pub frame: Option<Arc<Frame>>,
    tiles: Option<Vec<Level>>,
}
struct Level {
    index: usize,
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
    pub fn load(input: Input, swap: bool, limit: usize) -> Result<Arc<Self>> {
        let mut reader = input.reader()?;
        let format = ImageReader::new(&mut reader)
            .with_guessed_format()?
            .format()
            .or_else(|| input.path().and_then(|p| ImageFormat::from_path(p).ok()));
        if format == Some(ImageFormat::Tiff) {
            let mut tiff = Decoder::new(&mut reader)?;
            if tile_supported(&mut tiff)? {
                let size = tiff.dimensions()?;
                let mut levels = vec![Level {
                    index: 0,
                    width: size.0,
                    height: size.1,
                }];
                for index in 1..32 {
                    if !tiff.more_images() {
                        break;
                    }
                    tiff.next_image()?;
                    let reduced = tiff
                        .find_tag_unsigned::<u32>(Tag::NewSubfileType)?
                        .unwrap_or(0)
                        & 1
                        != 0;
                    let (w, h) = tiff.dimensions()?;
                    if reduced
                        && tile_supported(&mut tiff)?
                        && w <= size.0
                        && h <= size.1
                        && (f64::from(w) / f64::from(h) - f64::from(size.0) / f64::from(size.1))
                            .abs()
                            < 0.01
                    {
                        levels.push(Level {
                            index,
                            width: w,
                            height: h,
                        });
                    }
                }
                levels.sort_by_key(|l| std::cmp::Reverse(u64::from(l.width) * u64::from(l.height)));
                return Ok(Arc::new(Self {
                    input,
                    size,
                    animated: false,
                    orientation: Orientation::NoTransforms,
                    frame: None,
                    tiles: Some(levels),
                }));
            }
            drop(tiff);
            reader.rewind()?;
        }
        if let Some(format) = format {
            let decoder = ImageReader::with_format(&mut reader, format).into_decoder()?;
            let (w, h) = decoder.dimensions();
            if u64::from(w) * u64::from(h) * 4 > limit as u64 {
                return Err("image exceeds the decoded memory budget".into());
            }
            drop(decoder);
            reader.rewind()?;
        }
        let animated = match format {
            Some(ImageFormat::Gif) => true,
            Some(ImageFormat::WebP) => {
                image::codecs::webp::WebPDecoder::new(&mut reader)?.has_animation()
            }
            Some(ImageFormat::Png) => {
                image::codecs::png::PngDecoder::new(&mut reader)?.is_apng()?
            }
            _ => false,
        };
        reader.rewind()?;
        let pixels = Pixels::load(reader, input.path().map(Path::to_path_buf), limit)?;
        let size = (pixels.width as u32, pixels.height as u32);
        let orientation = pixels.orientation;
        let frame = pixels.freeze(swap)?;
        Ok(Arc::new(Self {
            input,
            size,
            animated,
            orientation,
            frame: Some(frame),
            tiles: None,
        }))
    }
    pub fn path(&self) -> Option<&Path> {
        self.input.path()
    }
    pub fn bytes(&self) -> usize {
        self.frame.as_ref().map_or(0, |f| f.bytes())
    }
    pub fn region_only(&self) -> bool {
        self.tiles.as_ref().is_some_and(|levels| {
            let l = levels.last().unwrap();
            u64::from(l.width) * u64::from(l.height) * 4 > REGION_BYTES as u64 / 2
        })
    }
    pub fn min_zoom(&self, size: (i32, i32)) -> f64 {
        if self.region_only() {
            ((f64::from(size.0) * f64::from(size.1) * 8.0) / REGION_BYTES as f64).sqrt()
        } else {
            0.0
        }
    }
    pub fn render(
        &self,
        view: View,
        animation: Option<&Arc<Frame>>,
        swap: bool,
    ) -> Result<Arc<Frame>> {
        let (sw, sh) = (f64::from(self.size.0), f64::from(self.size.1));
        let l = view.left.max(0.0);
        let t = view.top.max(0.0);
        let r = (view.left + view.span_width).min(sw);
        let b = (view.top + view.span_height).min(sh);
        let mut out = Pixels::new(view.width.try_into()?, view.height.try_into()?)?;
        if r <= l || b <= t {
            return out.freeze(false);
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
            return out.freeze(false);
        }
        let (frame, crop) = if let Some(levels) = &self.tiles {
            self.region(levels, (l, t, r, b), (w, h), swap)?
        } else {
            (
                animation
                    .or(self.frame.as_ref())
                    .ok_or("missing image pixels")?
                    .clone(),
                [l, t, r - l, b - t],
            )
        };
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
    fn region(
        &self,
        levels: &[Level],
        rect: (f64, f64, f64, f64),
        output: (u32, u32),
        swap: bool,
    ) -> Result<(Arc<Frame>, [f64; 4])> {
        let mut level = &levels[0];
        for candidate in levels {
            if (rect.2 - rect.0) * f64::from(candidate.width) / f64::from(self.size.0)
                >= f64::from(output.0)
                && (rect.3 - rect.1) * f64::from(candidate.height) / f64::from(self.size.1)
                    >= f64::from(output.1)
            {
                level = candidate;
            }
        }
        let sx = f64::from(level.width) / f64::from(self.size.0);
        let sy = f64::from(level.height) / f64::from(self.size.1);
        let l = rect.0 * sx;
        let t = rect.1 * sy;
        let r = rect.2 * sx;
        let b = rect.3 * sy;
        let halo =
            (3.0 * ((r - l) / f64::from(output.0)).max((b - t) / f64::from(output.1))).ceil() + 1.0;
        let left = (l - halo).floor().max(0.0) as u32;
        let top = (t - halo).floor().max(0.0) as u32;
        let right = (r + halo).ceil().min(f64::from(level.width)) as u32;
        let bottom = (b + halo).ceil().min(f64::from(level.height)) as u32;
        let (width, height) = (right - left, bottom - top);
        if u64::from(width) * u64::from(height) * 4 > REGION_BYTES as u64 {
            return Err("TIFF region is too large".into());
        }
        let mut out = Pixels::new(width.try_into()?, height.try_into()?)?;
        let mut decoder = Decoder::new(self.input.reader()?)?;
        decoder.seek_to_image(level.index)?;
        if decoder.dimensions()? != (level.width, level.height) || !tile_supported(&mut decoder)? {
            return Err("TIFF changed while open".into());
        }
        let (tw, th) = decoder.chunk_dimensions();
        let across = level.width.div_ceil(tw);
        let channels = match decoder.colortype()? {
            ColorType::RGB(8) => 3,
            ColorType::RGBA(8) => 4,
            ColorType::Gray(8) => 1,
            _ => return Err("unsupported TIFF tiles".into()),
        };
        let associated = decoder
            .find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)?
            .is_some_and(|v| v.contains(&1));
        for ty in top / th..=(bottom - 1) / th {
            for tx in left / tw..=(right - 1) / tw {
                let index = ty
                    .checked_mul(across)
                    .and_then(|i| i.checked_add(tx))
                    .ok_or("too many TIFF tiles")?;
                let (cw, ch) = decoder.chunk_data_dimensions(index);
                let DecodingResult::U8(data) = decoder.read_chunk(index)? else {
                    return Err("invalid TIFF tile samples".into());
                };
                if data.len() < cw as usize * ch as usize * channels {
                    return Err("short TIFF tile".into());
                }
                for yy in (ty * th).max(top)..((ty * th + ch).min(bottom)) {
                    for xx in (tx * tw).max(left)..((tx * tw + cw).min(right)) {
                        let p = ((yy - ty * th) as usize * cw as usize + (xx - tx * tw) as usize)
                            * channels;
                        let mut pixel = match channels {
                            1 => [data[p], data[p], data[p], 255],
                            3 => [data[p], data[p + 1], data[p + 2], 255],
                            _ => [data[p], data[p + 1], data[p + 2], data[p + 3]],
                        };
                        if !associated {
                            for c in 0..3 {
                                let n = u16::from(pixel[c]) * u16::from(pixel[3]) + 128;
                                pixel[c] = ((n + (n >> 8)) >> 8) as u8;
                            }
                        }
                        if swap {
                            pixel.swap(0, 2);
                        }
                        let q = ((yy - top) as usize * width as usize + (xx - left) as usize) * 4;
                        out.mapping[q..q + 4].copy_from_slice(&pixel);
                    }
                }
            }
        }
        Ok((
            out.freeze(false)?,
            [l - f64::from(left), t - f64::from(top), r - l, b - t],
        ))
    }
}
fn tile_supported<R: BufRead + Seek>(d: &mut Decoder<R>) -> Result<bool> {
    Ok(d.get_chunk_type() == ChunkType::Tile
        && matches!(
            d.colortype()?,
            ColorType::RGB(8) | ColorType::RGBA(8) | ColorType::Gray(8)
        )
        && d.find_tag_unsigned::<u16>(Tag::PlanarConfiguration)?
            .unwrap_or(1)
            == 1
        && d.find_tag_unsigned::<u16>(Tag::Orientation)?.unwrap_or(1) == 1)
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
        if source.bytes() > 16 * 1024 * 1024 {
            return Err("animation exceeds the frame memory budget".into());
        }
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
            d.set_limits(animation_limits())?;
            let n = d.loop_count();
            (d.into_frames(), n)
        }
        Some(ImageFormat::Png) => {
            let mut d = image::codecs::png::PngDecoder::new(reader)?;
            d.set_limits(animation_limits())?;
            let d = d.apng()?;
            let n = d.loop_count();
            (d.into_frames(), n)
        }
        Some(ImageFormat::WebP) => {
            let mut d = image::codecs::webp::WebPDecoder::new(reader)?;
            d.set_limits(animation_limits())?;
            let n = d.loop_count();
            (d.into_frames(), n)
        }
        _ => return Err("image is not animated".into()),
    };
    Ok((frames, count))
}

fn animation_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits
}
