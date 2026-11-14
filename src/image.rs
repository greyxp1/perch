use crate::Result;
use fast_image_resize::{
    MulDiv,
    PixelType::U8x4,
    images::{Image, ImageRef},
};
use image::{
    DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, metadata::Orientation,
};
use memmap2::{Mmap, MmapMut, MmapOptions};
use nix::{
    fcntl::{FcntlArg, SealFlag, fcntl},
    sys::memfd::{MFdFlags, memfd_create},
};
use spng::raw::ChunkAvail;
use std::{
    fs::File,
    io::{BufRead, Cursor, Read, Seek},
    os::fd::{AsRawFd, IntoRawFd},
    path::PathBuf,
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
            MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
        )?);
        file.set_len(size as u64)?;
        fcntl(
            &file,
            FcntlArg::F_ADD_SEALS(SealFlag::F_SEAL_GROW | SealFlag::F_SEAL_SHRINK),
        )?;
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
        path: Option<PathBuf>,
        limit: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let format = ImageReader::new(&mut reader)
            .with_guessed_format()?
            .format()
            .or_else(|| {
                path.as_ref()
                    .and_then(|path| ImageFormat::from_path(path).ok())
            });
        match format {
            Some(ImageFormat::Png) => Self::png(reader, cancelled),
            Some(format @ (ImageFormat::Jpeg | ImageFormat::WebP)) => {
                Self::native(reader, format, limit, 1920, cancelled)
            }
            Some(format) => {
                let (image, orientation) = decode(reader, format, limit)?;
                let mut pixels = Self::raster(image)?;
                pixels.orientation = orientation;
                Ok(pixels)
            }
            None => {
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
                if u64::from(size.width()) * u64::from(size.height()) * 4 > limit as u64 {
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
    fn png(reader: impl Read, cancelled: &dyn Fn() -> bool) -> Result<Self> {
        let mut decoder = spng::raw::RawContext::new()?;
        decoder.set_png_stream(reader)?;
        let info = decoder.get_ihdr()?;
        if u64::from(info.width) * u64::from(info.height) * 4 > crate::RENDER_BYTES as u64 / 2 {
            return Self::png_stream(
                decoder,
                (0, 0, info.width, info.height),
                crate::overview::fit((info.width, info.height), 2048),
                cancelled,
            );
        }
        let mut pixels = Self::new(info.width.try_into()?, info.height.try_into()?)?;
        decoder.decode_image(
            &mut pixels.mapping,
            spng::Format::Rgba8,
            spng::DecodeFlags::TRANSPARENCY,
        )?;
        decoder.decode_chunks()?;
        let orientation = decoder
            .get_exif()
            .chunk_avail()?
            .and_then(|exif| Orientation::from_exif_chunk(exif.data()))
            .unwrap_or(Orientation::NoTransforms);
        pixels.premultiply()?;
        pixels.orient(orientation)
    }
    pub fn png_region(
        reader: impl Read,
        expected: (u32, u32),
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let mut decoder = spng::raw::RawContext::new()?;
        decoder.set_png_stream(reader)?;
        let info = decoder.get_ihdr()?;
        if (info.width, info.height) != expected {
            return Err("image changed while open".into());
        }
        Self::png_stream(decoder, rect, size, cancelled)
    }
    fn png_stream<R: Read>(
        mut decoder: spng::raw::RawContext<R>,
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let info = decoder.get_ihdr()?;
        let mut raster = crate::overview::Rasterizer::new(rect, size, info.interlace_method == 0)?;
        let len = (info.width as usize)
            .checked_mul(4)
            .ok_or("PNG row is too large")?;
        let mut row = Vec::new();
        row.try_reserve_exact(len)?;
        row.resize(len, 0);
        decoder.decode_image(
            &mut [],
            spng::Format::Rgba8,
            spng::DecodeFlags::TRANSPARENCY | spng::DecodeFlags::PROGRESSIVE,
        )?;
        loop {
            if cancelled() {
                return Err("image loading cancelled".into());
            }
            let row_info = decoder.get_row_info()?;
            let done = match decoder.decode_row(&mut row) {
                Ok(()) => false,
                Err(spng::Error::Oi) => true,
                Err(e) => return Err(e.into()),
            };
            let y = row_info.row_num;
            if (rect.1..rect.3).contains(&y) {
                let (start, step) = if info.interlace_method == 0 {
                    (0, 1)
                } else {
                    let pass = usize::try_from(row_info.pass)?;
                    (
                        *[0, 4, 0, 2, 0, 1, 0].get(pass).ok_or("invalid PNG pass")?,
                        *[8, 8, 4, 4, 2, 2, 1].get(pass).unwrap(),
                    )
                };
                if step == 1 && info.interlace_method == 0 {
                    let data = &mut row[rect.0 as usize * 4..rect.2 as usize * 4];
                    MulDiv::default().multiply_alpha_inplace(&mut Image::from_slice_u8(
                        rect.2 - rect.0,
                        1,
                        data,
                        U8x4,
                    )?)?;
                    raster.row(rect.0, y, data)?;
                } else {
                    // Adam7 partitions pixels across passes. Retain only each pass's new pixels.
                    let first = rect.0 + (start + step - rect.0 % step) % step;
                    for x in (first..rect.2).step_by(step as usize) {
                        let mut pixel: [u8; 4] =
                            row[x as usize * 4..x as usize * 4 + 4].try_into().unwrap();
                        for c in 0..3 {
                            let n = u16::from(pixel[c]) * u16::from(pixel[3]) + 128;
                            pixel[c] = ((n + (n >> 8)) >> 8) as u8;
                        }
                        raster.pixel(x, y, pixel)?;
                    }
                }
            }
            if done {
                break;
            }
        }
        decoder.decode_chunks()?;
        let orientation = decoder
            .get_exif()
            .chunk_avail()?
            .and_then(|exif| Orientation::from_exif_chunk(exif.data()))
            .unwrap_or(Orientation::NoTransforms);
        let mut pixels = raster.finish()?;
        pixels.original_size = (info.width, info.height);
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
        let (dimensions, orientation) = encoded_info(&bytes, format)?;
        if dimensions != expected {
            return Err("image changed while open".into());
        }
        let mut pixels = if format == ImageFormat::Jpeg {
            Self::jpeg_stream(&bytes, expected, 1, rect, size, cancelled)?
        } else {
            let mut pixels = Self::new(size.0.try_into()?, size.1.try_into()?)?;
            let ok = unsafe {
                perch_webp_scaled(
                    bytes.as_ptr(),
                    bytes.len(),
                    pixels.mapping.as_mut_ptr(),
                    pixels.mapping.len(),
                    pixels.width,
                    pixels.height,
                    pixels.width * 4,
                    rect.0 as i32,
                    rect.1 as i32,
                    (rect.2 - rect.0) as i32,
                    (rect.3 - rect.1) as i32,
                )
            };
            if ok == 0 {
                return Err("cannot decode WebP region".into());
            }
            pixels.premultiply()?;
            pixels
        };
        pixels.original_size = expected;
        pixels.orient(orientation)
    }
    fn jpeg_stream(
        bytes: &[u8],
        expected: (u32, u32),
        denominator: u32,
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        struct Rows<'a> {
            raster: crate::overview::Rasterizer,
            cancelled: &'a dyn Fn() -> bool,
            error: Option<Box<dyn std::error::Error>>,
        }
        extern "C" fn row(
            context: *mut nix::libc::c_void,
            x: u32,
            y: u32,
            data: *const u8,
            width: u32,
        ) -> i32 {
            // The decoder calls synchronously, and its row is borrowed only for this call.
            let rows = unsafe { &mut *context.cast::<Rows<'_>>() };
            if (rows.cancelled)() {
                rows.error = Some("image loading cancelled".into());
                return 0;
            }
            if width == 0 {
                return 1;
            }
            let pixels = unsafe { std::slice::from_raw_parts(data, width as usize * 4) };
            if let Err(error) = rows.raster.row(x, y, pixels) {
                rows.error = Some(error);
                return 0;
            }
            1
        }
        let mut rows = Rows {
            raster: crate::overview::Rasterizer::new(rect, size, true)?,
            cancelled,
            error: None,
        };
        let ok = unsafe {
            perch_jpeg_rows(
                bytes.as_ptr(),
                bytes.len(),
                expected.0,
                expected.1,
                denominator,
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                row,
                (&mut rows as *mut Rows<'_>).cast(),
            )
        };
        if let Some(error) = rows.error {
            return Err(error);
        }
        if ok == 0 {
            return Err("cannot decode JPEG region".into());
        }
        let mut out = rows.raster.finish()?;
        out.original_size = expected;
        Ok(out)
    }
    fn premultiply(&mut self) -> Result<()> {
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
        pixels.original_size = if orientation.to_exif() >= 5 {
            (self.original_size.1, self.original_size.0)
        } else {
            self.original_size
        };
        Ok(pixels)
    }
    pub fn native(
        mut reader: impl BufRead + Seek,
        format: ImageFormat,
        limit: usize,
        max_side: u32,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        let ((w, h), orientation) = encoded_info(&bytes, format)?;
        if u64::from(w) * u64::from(h) * 4 > limit as u64 {
            return Err("image exceeds the decoded memory budget".into());
        }
        let (width, height) = if format == ImageFormat::Jpeg {
            // Decode enough DCT samples for the view and retain full-resolution coordinates.
            unsafe {
                let mut count = 0;
                let scales = tjGetScalingFactors(&mut count);
                if scales.is_null() || !(1..=64).contains(&count) {
                    return Err("cannot query JPEG scaling factors".into());
                }
                std::slice::from_raw_parts(scales, count as usize)
                    .iter()
                    .filter(|s| s.num > 0 && s.denom > 0 && s.num <= s.denom)
                    .map(|s| {
                        (
                            (u64::from(w) * s.num as u64).div_ceil(s.denom as u64) as u32,
                            (u64::from(h) * s.num as u64).div_ceil(s.denom as u64) as u32,
                        )
                    })
                    .filter(|(x, y)| (*x).max(*y) >= max_side.min(w.max(h)))
                    .min_by_key(|(x, y)| u64::from(*x) * u64::from(*y))
                    .unwrap_or((w, h))
            }
        } else if u64::from(w) * u64::from(h) * 4 > crate::RENDER_BYTES as u64 / 2 {
            crate::overview::fit((w, h), max_side)
        } else {
            (w, h)
        };
        if format == ImageFormat::Jpeg
            && u64::from(width) * u64::from(height) * 4 > crate::RENDER_BYTES as u64 / 2
        {
            let scaled = (w.div_ceil(8), h.div_ceil(8));
            match Self::jpeg_stream(
                &bytes,
                (w, h),
                8,
                (0, 0, scaled.0, scaled.1),
                crate::overview::fit(scaled, 2048),
                cancelled,
            ) {
                Ok(out) => return out.orient(orientation),
                Err(error) if cancelled() => return Err(error),
                Err(_) => {}
            }
        }
        let mut pixels = Self::new(width.try_into()?, height.try_into()?)?;
        pixels.original_size = (w, h);
        // Both codecs receive immutable input and a checked, full-size writable RGBA buffer.
        let decoded = unsafe {
            if format == ImageFormat::WebP {
                if (width, height) != (w, h) {
                    perch_webp_scaled(
                        bytes.as_ptr(),
                        bytes.len(),
                        pixels.mapping.as_mut_ptr(),
                        pixels.mapping.len(),
                        pixels.width,
                        pixels.height,
                        pixels.width * 4,
                        0,
                        0,
                        0,
                        0,
                    ) != 0
                } else {
                    !WebPDecodeRGBAInto(
                        bytes.as_ptr(),
                        bytes.len(),
                        pixels.mapping.as_mut_ptr(),
                        pixels.mapping.len(),
                        pixels.width * 4,
                    )
                    .is_null()
                }
            } else {
                let handle = tjInitDecompress();
                if handle.is_null() {
                    return Err("cannot initialize JPEG decoder".into());
                }
                let (mut width, mut height, mut sampling, mut colorspace) = (0, 0, 0, 0);
                let header = tjDecompressHeader3(
                    handle,
                    bytes.as_ptr(),
                    bytes.len() as _,
                    &mut width,
                    &mut height,
                    &mut sampling,
                    &mut colorspace,
                );
                let decoded = header == 0
                    && (width, height) == (w as i32, h as i32)
                    && tjDecompress2(
                        handle,
                        bytes.as_ptr(),
                        bytes.len() as _,
                        pixels.mapping.as_mut_ptr(),
                        pixels.width,
                        pixels.width * 4,
                        pixels.height,
                        7,
                        0,
                    ) == 0;
                tjDestroy(handle);
                decoded
            }
        };
        if !decoded {
            // Preserve animated WebP and JPEG color spaces unsupported by the native codec.
            drop(pixels);
            let (image, orientation) = decode(Cursor::new(bytes), format, limit)?;
            let mut pixels = Self::raster(image)?;
            pixels.orientation = orientation;
            return Ok(pixels);
        }
        if format == ImageFormat::WebP {
            pixels.premultiply()?;
        }
        pixels.orient(orientation)
    }
    pub fn tiff(file: File, size: (u32, u32)) -> Result<Self> {
        let mut pixels = Self::new(size.0.try_into()?, size.1.try_into()?)?;
        let tiff = Tiff::open(file)?;
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

fn encoded_info(bytes: &[u8], format: ImageFormat) -> Result<((u32, u32), Orientation)> {
    Ok(if format == ImageFormat::WebP {
        let (size, _) = webp_info(bytes)?;
        let mut orientation = Orientation::NoTransforms;
        let mut offset = 12_usize;
        let end = bytes
            .get(4..8)
            .map(|n| u32::from_le_bytes(n.try_into().unwrap()) as usize + 8)
            .ok_or("invalid WebP header")?
            .min(bytes.len());
        while let Some(header) = bytes
            .get(offset..offset.saturating_add(8))
            .filter(|_| offset + 8 <= end)
        {
            let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            let next = offset
                .checked_add(8)
                .and_then(|n| n.checked_add(length))
                .filter(|n| *n <= end)
                .ok_or("invalid WebP chunk")?;
            if &header[..4] == b"EXIF" {
                orientation = Orientation::from_exif_chunk(&bytes[offset + 8..next])
                    .unwrap_or(Orientation::NoTransforms);
            }
            offset = next.checked_add(length & 1).ok_or("invalid WebP chunk")?;
        }
        (size, orientation)
    } else {
        let mut header = ImageReader::with_format(Cursor::new(bytes), format);
        header.no_limits();
        let mut decoder = header.into_decoder()?;
        (decoder.dimensions(), decoder.orientation()?)
    })
}

pub fn webp_info(bytes: &[u8]) -> Result<((u32, u32), bool)> {
    let (mut w, mut h, mut animated) = (0, 0, 0);
    if unsafe { perch_webp_info(bytes.as_ptr(), bytes.len(), &mut w, &mut h, &mut animated) } == 0
        || w <= 0
        || h <= 0
    {
        return Err("invalid WebP header".into());
    }
    Ok(((w as u32, h as u32), animated != 0))
}

pub fn limits(bytes: usize) -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = (bytes != usize::MAX).then_some(bytes as u64);
    limits
}

pub fn tiff_limits(bytes: usize) -> tiff::decoder::Limits {
    let mut limits = tiff::decoder::Limits::default();
    limits.decoding_buffer_size = bytes;
    limits.intermediate_buffer_size = bytes;
    limits.ifd_value_size = bytes;
    limits
}

fn decode(
    mut reader: impl BufRead + Seek,
    format: ImageFormat,
    limit: usize,
) -> Result<(DynamicImage, Orientation)> {
    if format == ImageFormat::Tiff {
        let mut decoder = tiff::decoder::Decoder::new(&mut reader)?.with_limits(tiff_limits(limit));
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
                let mut image = DynamicImage::ImageRgb8(
                    image::RgbImage::from_raw(width, height, bytes).ok_or("invalid TIFF size")?,
                );
                image.apply_orientation(orientation);
                return Ok((image, orientation));
            }
            return Err("invalid TIFF samples".into());
        }
        drop(decoder);
        reader.rewind()?;
    }
    let mut reader = ImageReader::with_format(reader, format);
    reader.limits(limits(limit));
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok((image, orientation))
}
pub fn expand_rgb(source: &[u8], target: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("ssse3") {
        // Detection gates the only instruction set extension used by this implementation.
        unsafe {
            expand_rgb_ssse3(source, target);
        }
        return;
    }
    for (source, target) in source
        .as_chunks::<3>()
        .0
        .iter()
        .zip(target.as_chunks_mut::<4>().0)
    {
        target.copy_from_slice(&[source[0], source[1], source[2], 255]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn expand_rgb_ssse3(source: &[u8], target: &mut [u8]) {
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
    for (source, target) in source[read..]
        .as_chunks::<3>()
        .0
        .iter()
        .zip(target[write..].as_chunks_mut::<4>().0)
    {
        target.copy_from_slice(&[source[0], source[1], source[2], 255]);
    }
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
        fcntl(&self.file, FcntlArg::F_ADD_SEALS(SealFlag::F_SEAL_SEAL))?;
        Ok(Arc::new(Frame {
            file: self.file,
            width: self.width,
            height: self.height,
        }))
    }
}

#[link(name = "webpdecoder", kind = "static")]
unsafe extern "C" {
    fn perch_jpeg_rows(
        input: *const u8,
        length: usize,
        width: u32,
        height: u32,
        denominator: u32,
        left: u32,
        top: u32,
        right: u32,
        bottom: u32,
        row: extern "C" fn(*mut nix::libc::c_void, u32, u32, *const u8, u32) -> i32,
        context: *mut nix::libc::c_void,
    ) -> i32;
    fn perch_webp_info(
        input: *const u8,
        length: usize,
        width: *mut i32,
        height: *mut i32,
        animated: *mut i32,
    ) -> i32;
    fn perch_webp_scaled(
        input: *const u8,
        length: usize,
        output: *mut u8,
        size: usize,
        width: i32,
        height: i32,
        stride: i32,
        left: i32,
        top: i32,
        width_crop: i32,
        height_crop: i32,
    ) -> i32;
    fn WebPDecodeRGBAInto(
        input: *const u8,
        length: usize,
        output: *mut u8,
        size: usize,
        stride: i32,
    ) -> *mut u8;
}
#[link(name = "turbojpeg", kind = "static")]
unsafe extern "C" {
    fn tjInitDecompress() -> *mut nix::libc::c_void;
    fn tjDecompressHeader3(
        handle: *mut nix::libc::c_void,
        input: *const u8,
        length: nix::libc::c_ulong,
        width: *mut i32,
        height: *mut i32,
        sampling: *mut i32,
        colorspace: *mut i32,
    ) -> i32;
    fn tjDecompress2(
        handle: *mut nix::libc::c_void,
        input: *const u8,
        length: nix::libc::c_ulong,
        output: *mut u8,
        width: i32,
        stride: i32,
        height: i32,
        format: i32,
        flags: i32,
    ) -> i32;
    fn tjDestroy(handle: *mut nix::libc::c_void) -> i32;
}

#[repr(C)]
struct JpegScale {
    num: i32,
    denom: i32,
}
#[link(name = "turbojpeg", kind = "static")]
unsafe extern "C" {
    fn tjGetScalingFactors(count: *mut i32) -> *const JpegScale;
}

pub fn native_tiff(compression: u16) -> bool {
    unsafe { TIFFIsCODECConfigured(compression) != 0 }
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
pub struct Tiff(*mut nix::libc::c_void);
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
        let mut message = [0_i8; 1024];
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
        overview.finish()
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
#[link(name = "tiff", kind = "static")]
unsafe extern "C" {
    fn TIFFReadScanline(
        handle: *mut nix::libc::c_void,
        data: *mut nix::libc::c_void,
        row: u32,
        sample: u16,
    ) -> i32;
    fn TIFFReadRGBAStripExt(
        handle: *mut nix::libc::c_void,
        row: u32,
        data: *mut u32,
        stop: i32,
    ) -> i32;
    fn TIFFGetStrileOffset(handle: *mut nix::libc::c_void, index: u32) -> u64;
    fn TIFFGetStrileByteCount(handle: *mut nix::libc::c_void, index: u32) -> u64;
    fn TIFFCurrentDirOffset(handle: *mut nix::libc::c_void) -> u64;
    fn TIFFSetSubDirectory(handle: *mut nix::libc::c_void, offset: u64) -> i32;
    fn TIFFReadDirectory(handle: *mut nix::libc::c_void) -> i32;
    fn TIFFIsTiled(handle: *mut nix::libc::c_void) -> i32;
    fn TIFFRGBAImageOK(handle: *mut nix::libc::c_void, message: *mut i8) -> i32;
    fn TIFFReadRGBATileExt(
        handle: *mut nix::libc::c_void,
        x: u32,
        y: u32,
        data: *mut u32,
        stop: i32,
    ) -> i32;
    fn TIFFIsCODECConfigured(compression: u16) -> i32;
    fn TIFFFdOpen(
        fd: i32,
        name: *const nix::libc::c_char,
        mode: *const nix::libc::c_char,
    ) -> *mut nix::libc::c_void;
    fn TIFFGetField(handle: *mut nix::libc::c_void, tag: u32, ...) -> i32;
    fn TIFFSetField(handle: *mut nix::libc::c_void, tag: u32, ...) -> i32;
    fn TIFFReadRGBAImageOriented(
        handle: *mut nix::libc::c_void,
        width: u32,
        height: u32,
        data: *mut u32,
        orientation: i32,
        stop: i32,
    ) -> i32;
    fn TIFFClose(handle: *mut nix::libc::c_void);
}
