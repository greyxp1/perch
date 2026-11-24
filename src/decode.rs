use crate::{Result, shm::Pixels};
use image::{
    DynamicImage, Frame, Frames, ImageDecoder, ImageError, ImageFormat, ImageReader,
    error::{DecodingError, ImageFormatHint},
    metadata::{LoopCount, Orientation},
};
use std::{
    fs::File,
    io::{BufRead, Read, Seek},
    path::Path,
    sync::{Arc, OnceLock},
};

mod heif_avif;
mod jpeg_webp;
mod jxl;
mod png;
pub mod psd_raw;
mod tiff;

pub use self::tiff::Tiff;
pub use jpeg_webp::webp_info;

impl Pixels {
    pub fn load(
        mut reader: impl BufRead + Seek,
        format: Option<ImageFormat>,
        path: Option<&Path>,
        limit: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        match format {
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
            if let Some(pixels) = Self::tiff_rgb8(&mut reader, limit)? {
                return Ok(pixels);
            }
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
}

pub fn limits(bytes: usize) -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = (bytes != usize::MAX).then_some(bytes as u64);
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

#[derive(Clone, Copy)]
pub enum Kind {
    Jxl,
    Heif,
    Avif,
}

pub fn kind(reader: &mut (impl Read + Seek)) -> Result<Option<Kind>> {
    let mut header = [0; 16];
    let n = reader.read(&mut header)?;
    reader.rewind()?;
    Ok(
        if header[..n].starts_with(&[0xff, 0x0a])
            || header[..n].starts_with(b"\0\0\0\x0cJXL \r\n\x87\n")
        {
            Some(Kind::Jxl)
        } else if n >= 12
            && &header[4..8] == b"ftyp"
            && matches!(
                &header[8..12],
                b"avif"
                    | b"avis"
                    | b"heic"
                    | b"heix"
                    | b"hevc"
                    | b"hevx"
                    | b"mif1"
                    | b"mif2"
                    | b"msf1"
                    | b"jpgs"
                    | b"j2ki"
            )
        {
            Some(if matches!(&header[8..12], b"avif" | b"avis") {
                Kind::Avif
            } else {
                Kind::Heif
            })
        } else {
            None
        },
    )
}

fn error(e: impl ToString) -> ImageError {
    ImageError::Decoding(DecodingError::new(ImageFormatHint::Unknown, e.to_string()))
}

pub fn preview(file: File, kind: Kind, limit: usize) -> Result<(Pixels, bool)> {
    match kind {
        Kind::Jxl => jxl::preview(file, limit),
        Kind::Heif | Kind::Avif => heif_avif::preview(file, kind, limit),
    }
}

/// Converts the first frame of a sequence after releasing its decoder.
fn first_frame(mut frames: impl Iterator<Item = image::ImageResult<Frame>>) -> Result<Pixels> {
    let first = frames.next();
    drop(frames);
    let frame = first.ok_or("image has no displayable frames")??;
    Pixels::raster(DynamicImage::ImageRgba8(frame.into_buffer()))
}

pub fn frames(file: File, kind: Kind) -> Result<(Frames<'static>, LoopCount)> {
    match kind {
        Kind::Jxl => jxl::frames(file),
        Kind::Heif | Kind::Avif => heif_avif::frames(file),
    }
}
