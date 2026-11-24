use crate::{
    Result,
    codecs::{
        self, TIFF, TIFFClose, TIFFCurrentDirOffset, TIFFFdOpen, TIFFGetField,
        TIFFGetStrileByteCount, TIFFGetStrileOffset, TIFFIsCODECConfigured, TIFFIsTiled,
        TIFFRGBAImageOK, TIFFReadDirectory, TIFFReadRGBAImageOriented, TIFFReadRGBAStripExt,
        TIFFReadRGBATileExt, TIFFReadScanline, TIFFSetField, TIFFSetSubDirectory,
    },
    overview::Rasterizer,
};
use fast_image_resize::{
    MulDiv,
    PixelType::U8x4,
    images::{Image, ImageRef},
};
use image::{
    DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, metadata::Orientation,
};
use memmap2::{Mmap, MmapMut, MmapOptions};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
use std::{
    fs::File,
    io::{BufRead, Cursor, Read, Seek},
    os::fd::{AsRawFd, IntoRawFd},
    path::Path,
    sync::{Arc, OnceLock},
};

pub struct Pixels {
    pub file: File,
    pub mapping: MmapMut,
    pub width: i32,
    pub height: i32,
    pub orientation: Orientation,
    pub original_size: (u32, u32),
}
impl Pixels {
    pub fn new(width: i32, height: i32) -> Result<Self> {
        let size = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .filter(|_| width > 0 && height > 0)
            .ok_or("image is too large")?;
        let file = File::from(memfd_create(
            c"perch",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )?);
        file.set_len(size as u64)?;
        fcntl_add_seals(&file, SealFlags::GROW | SealFlags::SHRINK)?;
        // This private sealed file cannot shrink while mapped.
        let mapping = unsafe {
            MmapOptions::new()
                .len(size as usize)
                .populate()
                .map_mut(&file)?
        };
        Ok(Self {
            file,
            mapping,
            width,
            height,
            orientation: Orientation::NoTransforms,
            original_size: (width as u32, height as u32),
        })
    }
    pub fn load(
        mut reader: impl BufRead + Seek,
        format: Option<ImageFormat>,
        path: Option<&Path>,
        limit: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        match format {
            Some(ImageFormat::Png) => Self::png(reader, cancelled),
            Some(format @ (ImageFormat::Jpeg | ImageFormat::WebP)) => {
                Self::native(reader, format, limit, 1920, cancelled)
            }
            Some(format) => Self::decode(reader, format, limit),
            None => {
                {
                    match decoder(&mut reader, None, limit) {
                        Ok(decoder) => return Self::decoded(decoder),
                        Err(error) if error.downcast_ref::<image::ImageError>().is_some_and(|e| {
                            matches!(e, image::ImageError::Unsupported(u)
                                if matches!(u.kind(), image::error::UnsupportedErrorKind::Format(image::error::ImageFormatHint::Unknown)))
                        }) => {}
                        Err(error) => return Err(error),
                    }
                }
                {
                    let extension = path
                        .and_then(Path::extension)
                        .and_then(|e| e.to_str())
                        .map(str::to_ascii_lowercase);
                    let extra: Option<Box<dyn ImageDecoder + '_>> = match extension.as_deref() {
                        Some("ora") => {
                            Some(Box::new(image_extras::ora::OpenRasterDecoder::with_limits(
                                &mut reader,
                                limits(limit),
                            )?))
                        }
                        Some("otb") => {
                            Some(Box::new(image_extras::otb::OtbDecoder::new(&mut reader)?))
                        }
                        Some("wbmp") => {
                            Some(Box::new(image_extras::wbmp::WbmpDecoder::new(&mut reader)?))
                        }
                        Some("xbm" | "bm") => {
                            Some(Box::new(image_extras::xbm::XbmDecoder::new(&mut reader)?))
                        }
                        _ => None,
                    };
                    if let Some(mut decoder) = extra {
                        decoder.set_limits(limits(limit))?;
                        check_budget(&decoder, limit)?;
                        return Self::decoded(decoder);
                    }
                }
                reader.rewind()?;
                static FONTS: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes)?;
                let mut options = resvg::usvg::Options {
                    resources_dir: path
                        .and_then(|path| path.parent().map(std::path::Path::to_path_buf)),
                    ..Default::default()
                };
                let select_font = resvg::usvg::FontResolver::default_font_selector();
                options.font_resolver.select_font = Box::new(move |font, db| {
                    if db.is_empty() {
                        *db = FONTS
                            .get_or_init(|| {
                                let mut db = resvg::usvg::fontdb::Database::new();
                                db.load_system_fonts();
                                if let Some(font) = option_env!("PERCH_FALLBACK_FONT") {
                                    let _ = db.load_font_file(font);
                                }
                                Arc::new(db)
                            })
                            .clone();
                    }
                    select_font(font, db).or_else(|| {
                        db.query(&resvg::usvg::fontdb::Query {
                            families: &[resvg::usvg::fontdb::Family::Name("DejaVu Sans")],
                            ..Default::default()
                        })
                    })
                });
                let tree = resvg::usvg::Tree::from_data(&bytes, &options)?;
                let size = tree.size().to_int_size();
                if u64::from(size.width()) * u64::from(size.height()) > limit as u64 / 4 {
                    return Err("SVG exceeds the decoded memory budget".into());
                }
                let mut pixels = Self::new(size.width().try_into()?, size.height().try_into()?)?;
                let mut pixmap = resvg::tiny_skia::PixmapMut::from_bytes(
                    &mut pixels.mapping,
                    size.width(),
                    size.height(),
                )
                .ok_or("invalid SVG size")?;
                resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap);
                Ok(pixels)
            }
        }
    }
    fn decode(mut reader: impl BufRead + Seek, format: ImageFormat, limit: usize) -> Result<Self> {
        if format == ImageFormat::Tiff {
            let mut decoder =
                tiff::decoder::Decoder::new(&mut reader)?.with_limits(tiff_limits(limit));
            if decoder.colortype()? == tiff::ColorType::RGB(8)
                && decoder
                    .find_tag_unsigned::<u16>(tiff::tags::Tag::PlanarConfiguration)?
                    .unwrap_or(1)
                    == 1
            {
                let (width, height) = decoder.dimensions()?;
                let orientation = decoder
                    .find_tag(tiff::tags::Tag::Orientation)?
                    .and_then(|v| Orientation::from_exif(v.into_u16().ok()?.try_into().ok()?))
                    .unwrap_or(Orientation::NoTransforms);
                if let tiff::decoder::DecodingResult::U8(bytes) = decoder.read_image()? {
                    let image = DynamicImage::ImageRgb8(
                        image::RgbImage::from_raw(width, height, bytes)
                            .ok_or("invalid TIFF size")?,
                    );
                    let profile = decoder
                        .find_tag(tiff::tags::Tag::IccProfile)?
                        .map(|v| v.into_u8_vec())
                        .transpose()?;
                    return Self::oriented_raster(image, profile.as_deref(), orientation);
                }
                return Err("invalid TIFF samples".into());
            }
            drop(decoder);
            reader.rewind()?;
        }
        Self::decoded(decoder(reader, Some(format), limit)?)
    }

    fn decoded(mut decoder: impl ImageDecoder) -> Result<Self> {
        let orientation = decoder.orientation()?;
        let profile = decoder.icc_profile()?;
        let image = DynamicImage::from_decoder(decoder)?;
        Self::oriented_raster(image, profile.as_deref(), orientation)
    }

    fn oriented_raster(
        image: DynamicImage,
        profile: Option<&[u8]>,
        orientation: Orientation,
    ) -> Result<Self> {
        let mut image = crate::color::convert(image, profile)?;
        image.apply_orientation(orientation);
        let mut pixels = Self::raster(image)?;
        pixels.orientation = orientation;
        Ok(pixels)
    }

    fn png(reader: impl BufRead + Seek, cancelled: &dyn Fn() -> bool) -> Result<Self> {
        let mut png = Png::open(reader)?;
        let (width, height, _) = png.header();
        if u64::from(width) * u64::from(height) > crate::RENDER_BYTES as u64 / 8 {
            return Self::png_stream(
                png,
                (0, 0, width, height),
                crate::overview::fit((width, height), 2048),
                cancelled,
            );
        }
        let mut pixels = Self::new(width.try_into()?, height.try_into()?)?;
        png.decode(&mut pixels.mapping)?;
        let orientation = png.orientation()?;
        pixels.apply_color(png.color()?, false)?;
        pixels.premultiply()?;
        pixels.orient(orientation)
    }
    pub fn png_region(
        reader: impl BufRead + Seek,
        expected: (u32, u32),
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let png = Png::open(reader)?;
        let (width, height, _) = png.header();
        if (width, height) != expected {
            return Err("image changed while open".into());
        }
        Self::png_stream(png, rect, size, cancelled)
    }
    fn png_stream<R: BufRead + Seek>(
        mut png: Png<R>,
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let (width, height, interlaced) = png.header();
        let mut color = png.color()?;
        let mut raster = Rasterizer::new(rect, size, !interlaced)?;
        let len = (width as usize)
            .checked_mul(4)
            .ok_or("PNG row is too large")?;
        let mut row = Vec::new();
        row.try_reserve_exact(len)?;
        row.resize(len, 0);
        let alpha = MulDiv::default();
        loop {
            if cancelled() {
                return Err("image loading cancelled".into());
            }
            // Adam7 supplies contiguous samples at a fixed horizontal stride on each pass.
            let Some((y, start, step)) = png.row(&mut row)? else {
                break;
            };
            if (rect.1..rect.3).contains(&y) {
                let first = rect.0.saturating_sub(start).div_ceil(step);
                let end = rect.2.saturating_sub(start).div_ceil(step);
                if end > first {
                    let data = &mut row[first as usize * 4..end as usize * 4];
                    if let Some(color) = &mut color {
                        color.apply(data, false)?;
                    }
                    alpha.multiply_alpha_inplace(&mut Image::from_slice_u8(
                        end - first,
                        1,
                        data,
                        U8x4,
                    )?)?;
                    let x = start + first * step;
                    if step == 1 {
                        raster.row(x, y, data)?;
                    } else {
                        for (sample, &pixel) in data.as_chunks::<4>().0.iter().enumerate() {
                            raster.pixel(x + sample as u32 * step, y, pixel)?;
                        }
                    }
                }
            }
        }
        let orientation = png.orientation()?;
        let mut pixels = raster.finish()?;
        pixels.original_size = (width, height);
        pixels.orient(orientation)
    }
    pub fn encoded_region(
        mut reader: impl Read,
        format: ImageFormat,
        expected: (u32, u32),
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        if cancelled() {
            return Err("image loading cancelled".into());
        }
        Self::encoded(&bytes, format, |header| {
            if header.dimensions() != expected {
                return Err("image changed while open".into());
            }
            if format == ImageFormat::Jpeg {
                crate::jpeg::decode(&bytes, expected, (1, 1), rect, size, cancelled)
            } else {
                Self::webp(&bytes, rect, size, true, header)
            }
        })
    }
    pub fn apply_color(
        &mut self,
        mut color: Option<crate::color::Transform>,
        premultiplied: bool,
    ) -> Result<()> {
        crate::color::apply_rows(
            color.as_mut(),
            &mut self.mapping,
            self.width as usize,
            premultiplied,
        )
    }
    pub fn premultiply(&mut self) -> Result<()> {
        MulDiv::default().multiply_alpha_inplace(&mut Image::from_slice_u8(
            self.width as u32,
            self.height as u32,
            &mut self.mapping,
            U8x4,
        )?)?;
        Ok(())
    }
    pub fn orient(self, orientation: Orientation) -> Result<Self> {
        if orientation == Orientation::NoTransforms {
            return Ok(self);
        }
        let mut image = DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(
                self.width as u32,
                self.height as u32,
                self.mapping.to_vec(),
            )
            .ok_or("invalid image size")?,
        );
        image.apply_orientation(orientation);
        let (w, h) = image.dimensions();
        let mut pixels = Self::new(w.try_into()?, h.try_into()?)?;
        pixels.mapping.copy_from_slice(image.as_bytes());
        pixels.orientation = orientation;
        pixels.original_size = oriented(self.original_size, orientation);
        Ok(pixels)
    }
    fn native(
        mut reader: impl BufRead + Seek,
        format: ImageFormat,
        limit: usize,
        max_side: u32,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        let decoded = Self::encoded(&bytes, format, |header| {
            let (w, h) = header.dimensions();
            if u64::from(w) * u64::from(h) > limit as u64 / 4 {
                return Err("image exceeds the decoded memory budget".into());
            }
            if format == ImageFormat::Jpeg {
                // libjpeg-turbo supports every k/8 reduction from 1/8 through 1.
                let numerator = (1_u32..=8)
                    .find(|&n| {
                        (u64::from(w.max(h)) * u64::from(n)).div_ceil(8)
                            >= u64::from(max_side.min(w.max(h)))
                    })
                    .unwrap();
                let scaled = (
                    (u64::from(w) * u64::from(numerator)).div_ceil(8) as u32,
                    (u64::from(h) * u64::from(numerator)).div_ceil(8) as u32,
                );
                let size =
                    if u64::from(scaled.0) * u64::from(scaled.1) > crate::RENDER_BYTES as u64 / 8 {
                        crate::overview::fit(scaled, 2048)
                    } else {
                        scaled
                    };
                crate::jpeg::decode(
                    &bytes,
                    (w, h),
                    (numerator, 8),
                    (0, 0, scaled.0, scaled.1),
                    size,
                    cancelled,
                )
            } else {
                let size = if u64::from(w) * u64::from(h) > crate::RENDER_BYTES as u64 / 8 {
                    crate::overview::fit((w, h), max_side)
                } else {
                    (w, h)
                };
                Self::webp(&bytes, (0, 0, 0, 0), size, size != (w, h), header)
            }
        });
        match decoded {
            Err(_) if !cancelled() => Self::decode(Cursor::new(bytes), format, limit),
            decoded => decoded,
        }
    }

    // Decodes JPEG or WebP natively and applies the orientation from the encoded header.
    fn encoded(
        bytes: &[u8],
        format: ImageFormat,
        decode: impl FnOnce(&mut dyn ImageDecoder) -> Result<Self>,
    ) -> Result<Self> {
        let mut header = ImageReader::with_format(Cursor::new(bytes), format);
        header.no_limits();
        let mut header = header.into_decoder()?;
        let orientation = header.orientation()?;
        let size = header.dimensions();
        let mut pixels = decode(&mut header)?;
        pixels.original_size = size;
        pixels.orient(orientation)
    }

    fn webp(
        bytes: &[u8],
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        scaled: bool,
        header: &mut dyn ImageDecoder,
    ) -> Result<Self> {
        let mut pixels = Self::new(size.0.try_into()?, size.1.try_into()?)?;
        let crop = (
            i32::try_from(rect.0)?,
            i32::try_from(rect.1)?,
            i32::try_from(rect.2 - rect.0)?,
            i32::try_from(rect.3 - rect.1)?,
        );
        let mut config = std::mem::MaybeUninit::<codecs::WebPDecoderConfig>::uninit();
        let status = unsafe {
            if codecs::WebPInitDecoderConfigInternal(
                config.as_mut_ptr(),
                codecs::WEBP_DECODER_ABI_VERSION as i32,
            ) == 0
            {
                return Err("incompatible WebP decoder".into());
            }
            let mut config = config.assume_init();
            config.output.colorspace = codecs::WEBP_CSP_MODE_MODE_RGBA;
            config.output.is_external_memory = 1;
            config.output.u.RGBA = codecs::WebPRGBABuffer {
                rgba: pixels.mapping.as_mut_ptr(),
                stride: pixels.width * 4,
                size: pixels.mapping.len(),
            };
            if crop.2 != 0 {
                config.options.use_cropping = 1;
                (
                    config.options.crop_left,
                    config.options.crop_top,
                    config.options.crop_width,
                    config.options.crop_height,
                ) = crop;
            }
            config.options.use_scaling = i32::from(scaled);
            config.options.scaled_width = pixels.width;
            config.options.scaled_height = pixels.height;
            config.options.use_threads = 1;
            let status = codecs::WebPDecode(bytes.as_ptr(), bytes.len(), &mut config);
            codecs::WebPFreeDecBuffer(&mut config.output);
            status
        };
        if status != codecs::VP8StatusCode_VP8_STATUS_OK {
            return Err("cannot decode WebP".into());
        }
        pixels.apply_color(
            crate::color::Transform::new(header.icc_profile()?.as_deref())?,
            false,
        )?;
        pixels.premultiply()?;
        Ok(pixels)
    }

    pub fn tiff(tiff: Tiff, size: (u32, u32)) -> Result<Self> {
        let mut pixels = Self::new(size.0.try_into()?, size.1.try_into()?)?;
        let decoded = unsafe {
            let handle = tiff.0;
            tiff.dimensions()? == size
                && TIFFSetField(handle, 274, 1_i32) == 1
                && TIFFReadRGBAImageOriented(
                    handle,
                    size.0,
                    size.1,
                    pixels.mapping.as_mut_ptr().cast(),
                    1,
                    1,
                ) == 1
        };
        if !decoded {
            return Err("cannot decode TIFF pixels".into());
        }
        pixels.apply_color(tiff.color()?, true)?;
        Ok(pixels)
    }
    pub fn raster(image: DynamicImage) -> Result<Self> {
        let (width, height) = image.dimensions();
        let mut pixels = Self::new(width.try_into()?, height.try_into()?)?;
        if let DynamicImage::ImageRgb8(rgb) = image {
            expand_rgb(&rgb, &mut pixels.mapping);
        } else {
            let rgba = image.into_rgba8();
            MulDiv::default().multiply_alpha(
                &ImageRef::new(width, height, &rgba, U8x4)?,
                &mut Image::from_slice_u8(width, height, &mut pixels.mapping, U8x4)?,
            )?;
        }
        Ok(pixels)
    }
}

// Adam7 passes as first column, column step, first row and row step.
static ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 8, 0, 8),
    (4, 8, 0, 8),
    (0, 4, 4, 8),
    (2, 4, 0, 4),
    (0, 2, 2, 4),
    (1, 2, 0, 2),
    (0, 1, 1, 2),
];

/// PNG rows decoded to straight RGBA8.
struct Png<R: BufRead + Seek> {
    reader: png::Reader<R>,
    // The png crate does not expose the Adam7 position of each row.
    rows: Box<dyn Iterator<Item = (u32, u32, u32)>>,
}
impl<R: BufRead + Seek> Png<R> {
    fn open(reader: R) -> Result<Self> {
        let mut options = png::DecodeOptions::default();
        options.set_ignore_adler32(false);
        options.set_ignore_text_chunk(true);
        let mut decoder = png::Decoder::new_with_options(reader, options);
        // Perch budgets pixels itself, and the default limit rejects large metadata chunks.
        decoder.set_limits(png::Limits { bytes: usize::MAX });
        decoder.set_transformations(
            png::Transformations::EXPAND
                | png::Transformations::STRIP_16
                | png::Transformations::ALPHA,
        );
        let reader = decoder.read_info()?;
        let info = reader.info();
        let (width, height) = (info.width, info.height);
        let passes: &'static [_] = if info.interlaced {
            &ADAM7
        } else {
            &[(0, 1, 0, 1)]
        };
        let rows =
            passes
                .iter()
                .filter(move |pass| width > pass.0)
                .flat_map(move |&(x, dx, y, dy)| {
                    (y..height).step_by(dy as usize).map(move |y| (y, x, dx))
                });
        Ok(Self {
            reader,
            rows: Box::new(rows),
        })
    }
    fn header(&self) -> (u32, u32, bool) {
        let info = self.reader.info();
        (info.width, info.height, info.interlaced)
    }
    fn color(&self) -> Result<Option<crate::color::Transform>> {
        crate::color::Transform::new(self.reader.info().icc_profile.as_deref())
    }
    fn decode(&mut self, pixels: &mut [u8]) -> Result<()> {
        if self.reader.next_frame(pixels)?.color_type == png::ColorType::GrayscaleAlpha {
            expand_gray(pixels, pixels.len() / 4);
        }
        Ok(())
    }
    /// Decodes the next row and returns its image row, first column and column step.
    fn row(&mut self, row: &mut [u8]) -> Result<Option<(u32, u32, u32)>> {
        if self.reader.read_row(row)?.is_none() {
            return Ok(None);
        }
        let (y, x, dx) = self.rows.next().ok_or("invalid PNG rows")?;
        if self.reader.output_color_type().0 == png::ColorType::GrayscaleAlpha {
            expand_gray(row, (self.reader.info().width - x).div_ceil(dx) as usize);
        }
        Ok(Some((y, x, dx)))
    }
    fn orientation(&mut self) -> Result<Orientation> {
        // Later frames cannot change the APNG poster, so their damage must not reject it.
        if self.reader.info().animation_control.is_none() {
            self.reader.finish()?;
        } else {
            let _ = self.reader.finish();
        }
        Ok(self
            .reader
            .info()
            .exif_metadata
            .as_deref()
            .and_then(Orientation::from_exif_chunk)
            .unwrap_or(Orientation::NoTransforms))
    }
}

// Expanding from the end keeps unread gray-alpha samples intact.
fn expand_gray(data: &mut [u8], samples: usize) {
    for i in (0..samples).rev() {
        let (g, a) = (data[i * 2], data[i * 2 + 1]);
        data[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, a]);
    }
}

pub fn webp_info(bytes: &[u8]) -> Result<((u32, u32), bool)> {
    let mut features = std::mem::MaybeUninit::<codecs::WebPBitstreamFeatures>::uninit();
    let features = unsafe {
        if codecs::WebPGetFeaturesInternal(
            bytes.as_ptr(),
            bytes.len(),
            features.as_mut_ptr(),
            codecs::WEBP_DECODER_ABI_VERSION as i32,
        ) != codecs::VP8StatusCode_VP8_STATUS_OK
        {
            return Err("invalid WebP header".into());
        }
        features.assume_init()
    };
    if features.width <= 0 || features.height <= 0 {
        return Err("invalid WebP dimensions".into());
    }
    Ok((
        (features.width as u32, features.height as u32),
        features.has_animation != 0,
    ))
}

pub fn oriented((w, h): (u32, u32), orientation: Orientation) -> (u32, u32) {
    if orientation.to_exif() >= 5 {
        (h, w)
    } else {
        (w, h)
    }
}

pub fn limits(bytes: usize) -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = (bytes != usize::MAX).then_some(bytes as u64);
    limits
}

fn tiff_limits(bytes: usize) -> tiff::decoder::Limits {
    let mut limits = tiff::decoder::Limits::default();
    limits.decoding_buffer_size = bytes;
    limits.intermediate_buffer_size = bytes;
    limits.ifd_value_size = bytes;
    limits
}

pub fn decoder<'a>(
    reader: impl BufRead + Seek + 'a,
    format: Option<ImageFormat>,
    limit: usize,
) -> Result<impl ImageDecoder + 'a> {
    let mut image = match format {
        Some(format) => ImageReader::with_format(reader, format),
        None => ImageReader::new(reader).with_guessed_format()?,
    };
    image.limits(limits(limit));
    let decoder = image.into_decoder()?;
    check_budget(&decoder, limit)?;
    Ok(decoder)
}

fn check_budget(decoder: &impl ImageDecoder, limit: usize) -> Result<()> {
    let (w, h) = decoder.dimensions();
    if decoder.total_bytes() > limit as u64 || u64::from(w) * u64::from(h) > limit as u64 / 4 {
        return Err("image exceeds the decoded memory budget".into());
    }
    Ok(())
}

pub fn expand_rgb(source: &[u8], target: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    // Detection gates the only instruction set extension used by this implementation.
    let (read, write) = if is_x86_feature_detected!("ssse3") {
        unsafe { expand_rgb_ssse3(source, target) }
    } else {
        (0, 0)
    };
    #[cfg(not(target_arch = "x86_64"))]
    let (read, write) = (0, 0);
    for (source, target) in source[read..]
        .as_chunks::<3>()
        .0
        .iter()
        .zip(target[write..].as_chunks_mut::<4>().0)
    {
        target.copy_from_slice(&[source[0], source[1], source[2], 255]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn expand_rgb_ssse3(source: &[u8], target: &mut [u8]) -> (usize, usize) {
    use std::arch::x86_64::{
        _mm_loadu_si128, _mm_or_si128, _mm_set1_epi32, _mm_setr_epi8, _mm_shuffle_epi8,
        _mm_storeu_si128,
    };
    let mask = _mm_setr_epi8(0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11, -1);
    let alpha = _mm_set1_epi32(0xff000000_u32 as i32);
    let (mut read, mut write) = (0, 0);
    while source.len() - read >= 16 && target.len() - write >= 16 {
        // Each unaligned load/store fits its slice. Four source pixels consume 12 bytes.
        unsafe {
            let rgb = _mm_loadu_si128(source.as_ptr().add(read).cast());
            _mm_storeu_si128(
                target.as_mut_ptr().add(write).cast(),
                _mm_or_si128(_mm_shuffle_epi8(rgb, mask), alpha),
            );
        }
        read += 12;
        write += 16;
    }
    (read, write)
}

pub struct Frame {
    pub file: File,
    pub width: i32,
    pub height: i32,
}
impl Frame {
    pub fn bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
    pub fn map(&self) -> Result<Mmap> {
        // Frame lengths are sealed and Perch drops writable mappings before sharing.
        Ok(unsafe { MmapOptions::new().populate().map(&self.file)? })
    }
}
impl Pixels {
    pub fn freeze(mut self, swap: bool) -> Result<Arc<Frame>> {
        if swap {
            for pixel in self.mapping.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        drop(self.mapping);
        fcntl_add_seals(&self.file, SealFlags::SEAL)?;
        Ok(Arc::new(Frame {
            file: self.file,
            width: self.width,
            height: self.height,
        }))
    }
}

struct Chunk<'a> {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub solid: Option<[u8; 4]>,
    data: &'a [u32],
}
impl Chunk<'_> {
    pub fn row(&self, y: u32) -> &[u32] {
        let offset = (self.height - 1 - (y - self.y)) as usize * self.width as usize;
        &self.data[offset..offset + self.width as usize]
    }
}
pub struct Tiff(*mut TIFF);
impl Drop for Tiff {
    fn drop(&mut self) {
        unsafe { TIFFClose(self.0) }
    }
}
impl Tiff {
    pub fn open(file: File) -> Result<Self> {
        // Failed opens retain the fd, successful opens transfer it to TIFFClose.
        unsafe {
            let handle = TIFFFdOpen(file.as_raw_fd(), c"perch".as_ptr(), c"r".as_ptr());
            if handle.is_null() {
                return Err("cannot open TIFF decoder".into());
            }
            let _ = file.into_raw_fd();
            Ok(Self(handle))
        }
    }
    fn color(&self) -> Result<Option<crate::color::Transform>> {
        let (mut length, mut data) = (0_u32, std::ptr::null::<u8>());
        let mut photo = 0_u16;
        unsafe {
            TIFFGetField(self.0, 262, &mut photo);
        }
        if photo > 2 {
            return Ok(None);
        }
        if unsafe { TIFFGetField(self.0, 34675, &mut length, &mut data) } == 1 && !data.is_null() {
            // libtiff owns this profile until the current directory changes or closes.
            crate::color::Transform::new(Some(unsafe {
                std::slice::from_raw_parts(data, length as usize)
            }))
        } else {
            Ok(None)
        }
    }
    pub fn dimensions(&self) -> Result<(u32, u32)> {
        let (mut w, mut h) = (0_u32, 0_u32);
        if unsafe {
            TIFFGetField(self.0, 256, &mut w) == 1 && TIFFGetField(self.0, 257, &mut h) == 1
        } && w > 0
            && h > 0
        {
            Ok((w, h))
        } else {
            Err("invalid TIFF dimensions".into())
        }
    }
    pub fn orientation(&self) -> u16 {
        let mut value = 1_u16;
        unsafe {
            TIFFGetField(self.0, 274, &mut value);
        }
        value
    }
    pub fn offset(&self) -> u64 {
        unsafe { TIFFCurrentDirOffset(self.0) }
    }
    pub fn seek(&mut self, offset: u64) -> Result<()> {
        if unsafe { TIFFSetSubDirectory(self.0, offset) } == 1 {
            Ok(())
        } else {
            Err("cannot read TIFF directory".into())
        }
    }
    pub fn next(&mut self) -> bool {
        unsafe { TIFFReadDirectory(self.0) == 1 }
    }
    pub fn children(&self) -> Vec<u64> {
        let mut count = 0_u16;
        let mut offsets: *const u64 = std::ptr::null();
        if unsafe { TIFFGetField(self.0, 330, &mut count, &mut offsets) } == 1 && !offsets.is_null()
        {
            unsafe { std::slice::from_raw_parts(offsets, usize::from(count)).to_vec() }
        } else {
            Vec::new()
        }
    }
    pub fn reduced(&self) -> bool {
        let mut kind = 0_u32;
        unsafe {
            TIFFGetField(self.0, 254, &mut kind);
        }
        kind & 1 != 0
    }
    pub fn tiled(&self) -> bool {
        unsafe { TIFFIsTiled(self.0) != 0 }
    }
    pub fn readable(&self) -> bool {
        let mut message = [0 as std::ffi::c_char; 1024];
        let mut compression = 1_u16;
        unsafe {
            TIFFGetField(self.0, 259, &mut compression);
            TIFFIsCODECConfigured(compression) != 0
                && TIFFRGBAImageOK(self.0, message.as_mut_ptr()) == 1
        }
    }
    pub fn region(
        &mut self,
        rect: (u32, u32, u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Pixels> {
        let (left, top, right, bottom) = rect;
        let mut out = Pixels::new((right - left).try_into()?, (bottom - top).try_into()?)?;
        self.visit(rect, cancelled, |tile| {
            for y in tile.y.max(top)..tile.y.saturating_add(tile.height).min(bottom) {
                let start = tile.x.max(left);
                let end = tile.x.saturating_add(tile.width).min(right);
                let row = &tile.row(y)[(start - tile.x) as usize..(end - tile.x) as usize];
                let offset =
                    ((y - top) as usize * (right - left) as usize + (start - left) as usize) * 4;
                for (src, dst) in row.iter().zip(
                    out.mapping[offset..offset + row.len() * 4]
                        .as_chunks_mut::<4>()
                        .0,
                ) {
                    dst.copy_from_slice(&src.to_le_bytes());
                }
            }
            Ok(())
        })?;
        out.apply_color(self.color()?, true)?;
        Ok(out)
    }
    pub fn overview(
        &mut self,
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Pixels> {
        let mut overview = crate::overview::Overview::new(rect, size, !self.tiled())?;
        let mut row = Vec::new();
        self.visit(rect, cancelled, |tile| {
            if let Some(pixel) = tile.solid {
                overview.solid(
                    (
                        tile.x,
                        tile.y,
                        tile.x.saturating_add(tile.width),
                        tile.y.saturating_add(tile.height),
                    ),
                    pixel,
                );
            } else {
                row.resize(tile.width as usize * 4, 0);
                for y in tile.y.max(rect.1)..tile.y.saturating_add(tile.height).min(rect.3) {
                    for (src, dst) in tile.row(y).iter().zip(row.as_chunks_mut::<4>().0) {
                        dst.copy_from_slice(&src.to_le_bytes());
                    }
                    overview.row(tile.x, y, &row)?;
                }
            }
            Ok(())
        })?;
        let mut out = overview.finish()?;
        out.apply_color(self.color()?, true)?;
        Ok(out)
    }
    fn scanlines(
        &mut self,
        rect: (u32, u32, u32, u32),
        rows: u32,
        cancelled: &dyn Fn() -> bool,
        visit: &mut impl FnMut(Chunk<'_>) -> Result<()>,
    ) -> Result<bool> {
        let (mut bits, mut samples, mut photo, mut planar, mut sample_format) =
            (1_u16, 1_u16, 0_u16, 1_u16, 1_u16);
        unsafe {
            TIFFGetField(self.0, 258, &mut bits);
            TIFFGetField(self.0, 277, &mut samples);
            TIFFGetField(self.0, 262, &mut photo);
            TIFFGetField(self.0, 284, &mut planar);
            TIFFGetField(self.0, 339, &mut sample_format);
        }
        let colors = if photo == 2 { 3 } else { 1 };
        let alpha = if samples == colors + 1 {
            let mut count = 0_u16;
            let mut values: *const u16 = std::ptr::null();
            if unsafe { TIFFGetField(self.0, 338, &mut count, &mut values) } != 1
                || count != 1
                || values.is_null()
            {
                return Ok(false);
            }
            let kind = unsafe { *values };
            if !matches!(kind, 1 | 2) {
                return Ok(false);
            }
            kind
        } else if samples == colors {
            0
        } else {
            return Ok(false);
        };
        if planar != 1 || sample_format != 1 || !matches!(bits, 8 | 16) || photo > 2 {
            return Ok(false);
        }
        let width = self.dimensions()?.0;
        let stride = usize::from(samples) * usize::from(bits / 8);
        let len = (width as usize)
            .checked_mul(stride)
            .ok_or("TIFF row is too large")?;
        let mut row = Vec::new();
        row.try_reserve_exact(len)?;
        row.resize(len, 0_u8);
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(width as usize)?;
        rgba.resize(width as usize, 0_u32);
        for y in rect.1 / rows * rows..rect.3 {
            if cancelled() {
                return Err("image loading cancelled".into());
            }
            if unsafe { TIFFReadScanline(self.0, row.as_mut_ptr().cast(), y, 0) } != 1 {
                return Err("cannot decode TIFF row".into());
            }
            if y < rect.1 {
                continue;
            }
            for (src, dst) in row.chunks_exact(stride).zip(&mut rgba) {
                let sample = |c: usize| {
                    if bits == 8 {
                        src[c]
                    } else {
                        let n = u16::from_ne_bytes([src[c * 2], src[c * 2 + 1]]);
                        if photo == 2 || c == colors as usize {
                            ((u32::from(n) + 128) / 257) as u8
                        } else {
                            (n >> 8) as u8
                        }
                    }
                };
                let (r, g, b) = if colors == 3 {
                    (sample(0), sample(1), sample(2))
                } else {
                    let g = if photo == 0 {
                        255 - sample(0)
                    } else {
                        sample(0)
                    };
                    (g, g, g)
                };
                let a = if alpha == 0 {
                    255
                } else {
                    sample(colors as usize)
                };
                let mut pixel = [r, g, b, a];
                if alpha == 2 {
                    for channel in &mut pixel[..3] {
                        let n = u16::from(*channel) * u16::from(a) + 128;
                        *channel = ((n + (n >> 8)) >> 8) as u8;
                    }
                }
                *dst = u32::from_le_bytes(pixel);
            }
            let solid = rgba
                .iter()
                .all(|p| *p == rgba[0])
                .then(|| rgba[0].to_le_bytes());
            visit(Chunk {
                x: 0,
                y,
                width,
                height: 1,
                solid,
                data: &rgba,
            })?;
        }
        Ok(true)
    }
    fn visit(
        &mut self,
        rect: (u32, u32, u32, u32),
        cancelled: &dyn Fn() -> bool,
        mut visit: impl FnMut(Chunk<'_>) -> Result<()>,
    ) -> Result<()> {
        let (left, top, right, bottom) = rect;
        let (w, h) = self.dimensions()?;
        let tiled = self.tiled();
        let (mut tw, mut th, mut planar) = (w, h, 1_u16);
        if tiled {
            if unsafe {
                TIFFGetField(self.0, 322, &mut tw) != 1 || TIFFGetField(self.0, 323, &mut th) != 1
            } {
                return Err("invalid TIFF tile dimensions".into());
            }
        } else {
            unsafe {
                TIFFGetField(self.0, 278, &mut th);
            }
            th = th.min(h);
        }
        if tw == 0 || th == 0 {
            return Err("invalid TIFF chunk dimensions".into());
        }
        let bytes = u64::from(tw)
            .saturating_mul(u64::from(th))
            .saturating_mul(4);
        if bytes > isize::MAX as u64 {
            return Err("TIFF tile is too large".into());
        }
        if !tiled
            && bytes > crate::RENDER_BYTES as u64 / 2
            && self.scanlines(rect, th, cancelled, &mut visit)?
        {
            return Ok(());
        }
        let mut data = Vec::new();
        data.try_reserve_exact((bytes / 4) as usize)?;
        data.resize((bytes / 4) as usize, 0_u32);
        // Raw chunks are bottom-up associated RGBA. The gathered region is oriented once.
        if unsafe { TIFFSetField(self.0, 274, 1_i32) } != 1 {
            return Err("invalid TIFF orientation".into());
        }
        unsafe {
            TIFFGetField(self.0, 284, &mut planar);
        }
        let mut previous = None;
        let mut solid = None;
        for y in (top / th..=(bottom - 1) / th).map(|y| y * th) {
            for x in (left / tw..=(right - 1) / tw).map(|x| x * tw) {
                if cancelled() {
                    return Err("image loading cancelled".into());
                }
                let index = (y / th)
                    .checked_mul(w.div_ceil(tw))
                    .and_then(|i| i.checked_add(x / tw))
                    .ok_or("too many TIFF tiles")?;
                let key = unsafe {
                    (
                        TIFFGetStrileOffset(self.0, index),
                        TIFFGetStrileByteCount(self.0, index),
                        tw.min(w - x),
                        th.min(h - y),
                    )
                };
                // Sparse TIFFs may reference the same encoded tile many times.
                if planar != 1 || previous != Some(key) {
                    let decoded = unsafe {
                        if tiled {
                            TIFFReadRGBATileExt(self.0, x, y, data.as_mut_ptr(), 1)
                        } else {
                            TIFFReadRGBAStripExt(self.0, y, data.as_mut_ptr(), 1)
                        }
                    };
                    if decoded != 1 {
                        return Err("cannot decode TIFF chunk".into());
                    }
                    let len = if tiled {
                        data.len()
                    } else {
                        tw as usize * th.min(h - y) as usize
                    };
                    solid = data[..len]
                        .iter()
                        .all(|p| *p == data[0])
                        .then(|| data[0].to_le_bytes());
                    previous = Some(key);
                }
                visit(Chunk {
                    x,
                    y,
                    width: tw,
                    height: if tiled { th } else { th.min(h - y) },
                    solid,
                    data: &data,
                })?;
            }
        }
        Ok(())
    }
}
