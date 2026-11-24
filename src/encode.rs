use crate::{Result, decode::Kind, shm::Pixels};
use fast_image_resize::{
    MulDiv,
    PixelType::U8x4,
    Resizer,
    images::{Image, ImageRef},
};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, metadata::Orientation};
use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};
use std::{io::Cursor, path::Path};

mod avif;
pub mod cli;
mod jpeg;
mod jxl;
mod png;
mod raster;
mod webp;

pub use png::fast as fast_png;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    WebP,
    Tiff,
    Bmp,
    Gif,
    Ico,
    Qoi,
    Avif,
    Jxl,
}
impl Format {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "png" => Self::Png,
            "jpg" | "jpeg" => Self::Jpeg,
            "webp" => Self::WebP,
            "tif" | "tiff" => Self::Tiff,
            "bmp" => Self::Bmp,
            "gif" => Self::Gif,
            "ico" => Self::Ico,
            "qoi" => Self::Qoi,
            "avif" => Self::Avif,
            "jxl" => Self::Jxl,
            _ => return None,
        })
    }
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        match crate::decode::kind(&mut Cursor::new(bytes)).ok()? {
            Some(Kind::Avif) => return Some(Self::Avif),
            Some(Kind::Jxl) => return Some(Self::Jxl),
            _ => {}
        }
        Some(match image::guess_format(bytes).ok()? {
            ImageFormat::Png => Self::Png,
            ImageFormat::Jpeg => Self::Jpeg,
            ImageFormat::WebP => Self::WebP,
            ImageFormat::Tiff => Self::Tiff,
            ImageFormat::Bmp => Self::Bmp,
            ImageFormat::Gif => Self::Gif,
            ImageFormat::Ico => Self::Ico,
            ImageFormat::Qoi => Self::Qoi,
            _ => return None,
        })
    }
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
            Self::Tiff => "tiff",
            Self::Bmp => "bmp",
            Self::Gif => "gif",
            Self::Ico => "ico",
            Self::Qoi => "qoi",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
        }
    }
}

/// Target size of a resize, which always preserves the aspect ratio.
#[derive(Clone, Copy)]
pub enum Resize {
    Fit(u32, u32),
    Width(u32),
    Height(u32),
    Percent(f64),
}
impl Resize {
    pub fn parse(text: &str) -> Option<Self> {
        let resize = if let Some(percent) = text.strip_suffix('%') {
            Self::Percent(
                percent
                    .parse()
                    .ok()
                    .filter(|p: &f64| p.is_finite() && *p > 0.0)?,
            )
        } else {
            let (w, h) = text.split_once(['x', 'X'])?;
            let side = |s: &str| s.parse().ok().filter(|&n| n > 0);
            match (w.is_empty(), h.is_empty()) {
                (false, false) => Self::Fit(side(w)?, side(h)?),
                (false, true) => Self::Width(side(w)?),
                (true, false) => Self::Height(side(h)?),
                (true, true) => return None,
            }
        };
        Some(resize)
    }
    fn size(self, (w, h): (u32, u32)) -> Result<(u32, u32)> {
        let (w, h) = (f64::from(w), f64::from(h));
        let scale = match self {
            Self::Fit(fw, fh) => (f64::from(fw) / w).min(f64::from(fh) / h),
            Self::Width(fw) => f64::from(fw) / w,
            Self::Height(fh) => f64::from(fh) / h,
            Self::Percent(p) => p / 100.0,
        };
        let side = |n: f64| -> Result<u32> {
            let n = (n * scale).round().max(1.0);
            (n <= f64::from(u32::MAX))
                .then_some(n as u32)
                .ok_or_else(|| "the resized image is too large".into())
        };
        Ok((side(w)?, side(h)?))
    }
}

pub struct ConvertOptions {
    pub format: Format,
    pub quality: u8,
    pub lossless: bool,
    pub resize: Option<Resize>,
    pub background: [u8; 3],
    pub keep_metadata: bool,
}

pub struct CompressOptions {
    pub lossy: bool,
    pub quality: u8,
    pub keep_metadata: bool,
}

/// Upright straight RGBA8 pixels in the color space of `icc`, or sRGB without a profile.
#[derive(Clone)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub icc: Option<Vec<u8>>,
    /// TIFF-structured EXIF whose orientation is already applied to the pixels.
    pub exif: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
}
impl Raster {
    /// Decodes the first frame, using the viewer's decoders for formats the image crate lacks.
    pub fn load(bytes: &[u8], path: Option<&Path>) -> Result<Self> {
        let format = image::guess_format(bytes).ok();
        if matches!(format, None | Some(ImageFormat::Avif)) {
            match crate::decode::kind(&mut Cursor::new(bytes))? {
                Some(Kind::Jxl) => return jxl::load(bytes),
                Some(Kind::Heif | Kind::Avif) => return avif::load(bytes),
                None => {}
            }
        }
        if matches!(format, None | Some(ImageFormat::Tiff))
            && let Some(path) = path
            && let Some(pixels) =
                crate::decode::psd_raw::load(std::fs::File::open(path)?, usize::MAX, &|| false)?
        {
            return Self::premultiplied(pixels);
        }
        match format {
            Some(format) => Self::decode(bytes, format),
            None => Self::premultiplied(Pixels::load(
                Cursor::new(bytes),
                None,
                path,
                usize::MAX,
                &|| false,
            )?),
        }
    }

    fn decode(bytes: &[u8], format: ImageFormat) -> Result<Self> {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        reader.no_limits();
        let mut decoder = reader.into_decoder()?;
        let icc = decoder.icc_profile()?;
        let xmp = decoder.xmp_metadata()?;
        let mut exif = decoder
            .exif_metadata()?
            .map(|exif| match exif.strip_prefix(b"Exif\0\0") {
                Some(tiff) => tiff.to_vec(),
                None => exif,
            });
        // The WebP decoder misses orientations behind an Exif prefix.
        let orientation = match exif.as_deref().and_then(Orientation::from_exif_chunk) {
            Some(orientation) => orientation,
            None => decoder.orientation()?,
        };
        if let Some(exif) = &mut exif {
            let _ = Orientation::remove_from_exif_chunk(exif);
        }
        let space = icc
            .as_deref()
            .and_then(|icc| ColorProfile::new_from_slice(icc).ok())
            .map(|profile| profile.color_space);
        let mut raster = if format == ImageFormat::Jpeg
            && (space == Some(DataColorSpace::Cmyk) || jpeg::cmyk(bytes))
        {
            // The image crate converts CMYK without its profile or Adobe inversion.
            drop(decoder);
            Self::premultiplied(Pixels::jpeg(bytes)?)?
        } else {
            let mut image = DynamicImage::from_decoder(decoder)?;
            image.apply_orientation(orientation);
            Self {
                width: image.width(),
                height: image.height(),
                rgba: image.into_rgba8().into_raw(),
                icc,
                exif: None,
                xmp: None,
            }
        };
        raster.fit_profile()?;
        raster.exif = exif;
        raster.xmp = xmp;
        Ok(raster)
    }

    /// Takes sRGB pixels from the viewer's decoders.
    fn premultiplied(pixels: Pixels) -> Result<Self> {
        let (width, height) = (pixels.width as u32, pixels.height as u32);
        let mut rgba = pixels.mapping.to_vec();
        MulDiv::default()
            .divide_alpha_inplace(&mut Image::from_slice_u8(width, height, &mut rgba, U8x4)?)?;
        Ok(Self {
            width,
            height,
            rgba,
            icc: None,
            exif: None,
            xmp: None,
        })
    }

    /// Keeps RGB profiles, converting gray ones to sRGB and dropping others.
    fn fit_profile(&mut self) -> Result<()> {
        let space = (self.icc.as_deref())
            .and_then(|icc| ColorProfile::new_from_slice(icc).ok())
            .map(|profile| profile.color_space);
        match space {
            // Gray profiles cannot describe the RGB samples every encoder receives.
            Some(DataColorSpace::Gray) => self.convert_srgb(),
            Some(DataColorSpace::Rgb) | None => Ok(()),
            Some(_) => {
                self.icc = None;
                Ok(())
            }
        }
    }

    /// Converts the pixels to sRGB and drops the profile.
    pub fn convert_srgb(&mut self) -> Result<()> {
        if let Some(icc) = self.icc.take() {
            let mut transform = crate::color::Transform::new(Some(&icc))?;
            crate::color::apply_rows(
                transform.as_mut(),
                &mut self.rgba,
                self.width as usize,
                false,
            )?;
        }
        Ok(())
    }

    pub fn opaque(&self) -> bool {
        self.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255)
    }

    pub fn rgb(&self) -> Vec<u8> {
        self.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect()
    }

    /// Composites onto an sRGB background, converted into the profile when it has an inverse.
    pub fn flatten(&mut self, background: [u8; 3]) -> Result<()> {
        let mut color = background;
        if let Some(icc) = &self.icc {
            let converted = ColorProfile::new_from_slice(icc).and_then(|profile| {
                ColorProfile::new_srgb()
                    .create_transform_8bit(
                        Layout::Rgb,
                        &profile,
                        Layout::Rgb,
                        TransformOptions::default(),
                    )?
                    .transform(&background, &mut color)
            });
            if converted.is_err() {
                color = background;
                self.convert_srgb()?;
            }
        }
        for p in self.rgba.as_chunks_mut::<4>().0 {
            let a = u32::from(p[3]);
            for c in 0..3 {
                p[c] = ((u32::from(p[c]) * a + u32::from(color[c]) * (255 - a) + 127) / 255) as u8;
            }
            p[3] = 255;
        }
        Ok(())
    }

    pub fn resize(&mut self, resize: Resize) -> Result<()> {
        let (width, height) = resize.size((self.width, self.height))?;
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        let mut rgba = Vec::new();
        (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .and_then(|len| rgba.try_reserve_exact(len).ok())
            .ok_or("the resized image is too large")?;
        rgba.resize(rgba.capacity(), 0);
        let mut target = Image::from_vec_u8(width, height, rgba, U8x4)?;
        Resizer::new().resize(
            &ImageRef::new(self.width, self.height, &self.rgba, U8x4)?,
            &mut target,
            None,
        )?;
        (self.width, self.height, self.rgba) = (width, height, target.into_vec());
        Ok(())
    }

    pub fn encode(&self, format: Format, quality: u8, lossless: bool) -> Result<Vec<u8>> {
        match format {
            Format::Png => png::encode(self),
            Format::Jpeg => jpeg::encode(self, quality),
            Format::WebP => webp::encode(self, quality, lossless),
            Format::Tiff => raster::tiff(self, raster::UNASSOCIATED_ALPHA),
            Format::Bmp => raster::bmp(self),
            Format::Gif => raster::gif(self),
            Format::Ico => raster::ico(self),
            Format::Qoi => raster::qoi(self),
            Format::Avif => avif::encode(self, quality, lossless),
            Format::Jxl => jxl::encode(self, quality, lossless),
        }
    }

    /// Clones the pixels as sRGB for formats without color profiles.
    fn srgb(&self) -> Result<std::borrow::Cow<'_, Self>> {
        Ok(if self.icc.is_some() {
            let mut raster = self.clone();
            raster.convert_srgb()?;
            std::borrow::Cow::Owned(raster)
        } else {
            std::borrow::Cow::Borrowed(self)
        })
    }
}

pub fn convert(mut raster: Raster, options: &ConvertOptions) -> Result<Vec<u8>> {
    if !options.keep_metadata {
        (raster.exif, raster.xmp) = (None, None);
    }
    if options.lossless && options.format == Format::Jpeg {
        return Err("JPEG has no lossless mode".into());
    }
    if let Some(resize) = options.resize {
        raster.resize(resize)?;
    }
    if matches!(options.format, Format::Jpeg | Format::Bmp) && !raster.opaque() {
        // BMP has no profile, so blending in sRGB keeps the background exact.
        if options.format == Format::Bmp {
            raster.convert_srgb()?;
        }
        raster.flatten(options.background)?;
    }
    if options.format == Format::Ico && raster.width.max(raster.height) > 256 {
        return Err(format!(
            "ICO is limited to 256x256 but the image is {}x{}, use --resize 256x256",
            raster.width, raster.height
        )
        .into());
    }
    raster.encode(options.format, options.quality, options.lossless)
}

/// Returns a smaller encoding of the same format, or `None` when the original is already smaller.
pub fn compress(bytes: &[u8], options: &CompressOptions) -> Result<Option<Vec<u8>>> {
    let decode = || -> Result<Raster> {
        let mut raster = Raster::decode(bytes, image::guess_format(bytes)?)?;
        if !options.keep_metadata {
            (raster.exif, raster.xmp) = (None, None);
        }
        Ok(raster)
    };
    let keep = options.keep_metadata;
    let output = match Format::detect(bytes) {
        Some(Format::Png) if options.lossy && !png::animated(bytes) => {
            png::quantize(&decode()?, options.quality)?
        }
        Some(Format::Png) => png::optimize(bytes, keep)?,
        Some(Format::Jpeg) if options.lossy => {
            jpeg::check(bytes)?;
            jpeg::encode(&decode()?, options.quality)?
        }
        Some(Format::Jpeg) => jpeg::transcode(bytes, keep)?,
        Some(Format::WebP) => webp::compress(bytes, options, decode)?,
        Some(Format::Avif) => avif::compress(bytes, options)?,
        Some(Format::Jxl) => jxl::compress(bytes, options)?,
        Some(Format::Tiff) => {
            let alpha = raster::tiff_layout(bytes)?;
            raster::tiff(&decode()?, alpha)?
        }
        _ => return Err("only PNG, JPEG, WebP, TIFF, AVIF and JXL can be compressed".into()),
    };
    Ok((output.len() < bytes.len()).then_some(output))
}

/// Builds the smallest EXIF that keeps a non-identity orientation.
fn orientation_exif(orientation: Option<Orientation>) -> Option<Vec<u8>> {
    let value = orientation.map_or(1, Orientation::to_exif);
    (value != 1).then(|| {
        let mut exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
        exif.extend_from_slice(&[value, 0, 0, 0, 0, 0, 0, 0]);
        exif
    })
}
