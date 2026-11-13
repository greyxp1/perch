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
    io::{BufRead, Read, Seek},
    path::PathBuf,
    sync::{Arc, OnceLock},
};

pub struct Pixels {
    pub file: File,
    pub mapping: MmapMut,
    pub width: i32,
    pub height: i32,
    pub orientation: Orientation,
}
impl Pixels {
    pub fn new(width: i32, height: i32) -> Result<Self> {
        let size = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .filter(|n| width > 0 && height > 0 && *n <= 256 * 1024 * 1024)
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
        let mapping = unsafe { MmapOptions::new().len(size as usize).map_mut(&file)? };
        Ok(Self {
            file,
            mapping,
            width,
            height,
            orientation: Orientation::NoTransforms,
        })
    }
    pub fn load(
        mut reader: impl BufRead + Seek,
        path: Option<PathBuf>,
        limit: usize,
    ) -> Result<Self> {
        let format = ImageReader::new(&mut reader)
            .with_guessed_format()?
            .format()
            .or_else(|| {
                path.as_ref()
                    .and_then(|path| ImageFormat::from_path(path).ok())
            });
        match format {
            Some(ImageFormat::Png) => Self::png(reader),
            Some(format) => {
                let (image, orientation) = decode(reader, format)?;
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
    fn png(reader: impl Read) -> Result<Self> {
        let mut decoder = spng::raw::RawContext::new()?;
        decoder.set_png_stream(reader)?;
        let info = decoder.get_ihdr()?;
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
        if orientation != Orientation::NoTransforms {
            let mut image = DynamicImage::ImageRgba8(
                image::RgbaImage::from_raw(info.width, info.height, pixels.mapping.to_vec())
                    .ok_or("invalid PNG size")?,
            );
            image.apply_orientation(orientation);
            let mut pixels = Self::raster(image)?;
            pixels.orientation = orientation;
            return Ok(pixels);
        }
        MulDiv::default().multiply_alpha_inplace(&mut Image::from_slice_u8(
            info.width,
            info.height,
            &mut pixels.mapping,
            U8x4,
        )?)?;
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

fn decode(
    mut reader: impl BufRead + Seek,
    format: ImageFormat,
) -> Result<(DynamicImage, Orientation)> {
    if format == ImageFormat::Tiff {
        let mut decoder = tiff::decoder::Decoder::new(&mut reader)?;
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
    let mut decoder = ImageReader::with_format(reader, format).into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok((image, orientation))
}
fn expand_rgb(source: &[u8], target: &mut [u8]) {
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
        Ok(unsafe { MmapOptions::new().map(&self.file)? })
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
