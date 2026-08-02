use crate::{Result, image::Pixels};
use image::{
    Delay, Frame, Frames, ImageError, RgbaImage,
    error::{DecodingError, ImageFormatHint},
    metadata::LoopCount,
};
use libheif_sys as heif;
use std::{
    ffi::{CStr, CString, c_void},
    fs::File,
    io::{BufReader, Read, Seek},
    num::NonZeroU32,
    os::fd::AsRawFd,
    ptr::{NonNull, null, null_mut},
    sync::OnceLock,
    time::Duration,
};

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

struct JxlFrames {
    image: jxl_oxide::JxlImage,
    index: usize,
    reader: BufReader<File>,
    buffer: [u8; 4096],
    valid: usize,
    ended: bool,
}
impl JxlFrames {
    fn new(file: File, limit: usize) -> Result<Self> {
        let mut reader = BufReader::new(file);
        let mut uninit = jxl_oxide::JxlImage::builder().build_uninit();
        let mut buffer = [0; 4096];
        let mut valid = 0;
        let mut image = loop {
            let n = reader.read(&mut buffer[valid..])?;
            if n == 0 {
                return Err("incomplete JPEG XL header".into());
            }
            valid += n;
            let consumed = uninit.feed_bytes(&buffer[..valid]).map_err(error)?;
            buffer.copy_within(consumed..valid, 0);
            valid -= consumed;
            match uninit.try_init().map_err(error)? {
                jxl_oxide::InitializeResult::NeedMoreData(next) => uninit = next,
                jxl_oxide::InitializeResult::Initialized(image) => break image,
            }
        };
        if u64::from(image.width()) * u64::from(image.height()) > limit as u64 / 4 {
            return Err("image exceeds the decoded memory budget".into());
        }
        image.set_cms(jxl_oxide::Moxcms);
        image.request_color_encoding(jxl_oxide::EnumColourEncoding::srgb(
            jxl_oxide::RenderingIntent::Relative,
        ));
        Ok(Self {
            image,
            index: 0,
            reader,
            buffer,
            valid,
            ended: false,
        })
    }
    fn loops(&self) -> LoopCount {
        self.image
            .image_header()
            .metadata
            .animation
            .as_ref()
            .and_then(|a| NonZeroU32::new(a.num_loops))
            .map_or(LoopCount::Infinite, LoopCount::Finite)
    }
}
impl Iterator for JxlFrames {
    type Item = image::ImageResult<Frame>;
    fn next(&mut self) -> Option<Self::Item> {
        while self.index >= self.image.num_loaded_keyframes() {
            if self.ended || self.image.is_loading_done() {
                return None;
            }
            let result = (|| -> Result<()> {
                let n = self.reader.read(&mut self.buffer[self.valid..])?;
                if n == 0 {
                    self.ended = true;
                    self.image.finalize().map_err(error)?;
                    if self.index >= self.image.num_loaded_keyframes() {
                        return Err("incomplete JPEG XL frame".into());
                    }
                } else {
                    self.valid += n;
                    let consumed = self
                        .image
                        .feed_bytes(&self.buffer[..self.valid])
                        .map_err(error)?;
                    self.buffer.copy_within(consumed..self.valid, 0);
                    self.valid -= consumed;
                }
                Ok(())
            })();
            if let Err(e) = result {
                self.ended = true;
                return Some(Err(error(e)));
            }
        }
        let index = self.index;
        self.index += 1;
        Some(
            (|| -> Result<Frame> {
                let render = self.image.render_frame(index).map_err(error)?;
                let mut stream = render.stream();
                let (w, h, channels) =
                    (stream.width(), stream.height(), stream.channels() as usize);
                let len = (w as usize)
                    .checked_mul(h as usize)
                    .and_then(|n| n.checked_mul(channels))
                    .ok_or("JPEG XL frame is too large")?;
                let mut samples = Vec::new();
                samples.try_reserve_exact(len)?;
                samples.resize(len, 0u8);
                if stream.write_to_buffer(&mut samples) != len {
                    return Err("incomplete JPEG XL frame".into());
                }
                let rgba = if channels == 4 {
                    RgbaImage::from_raw(w, h, samples).ok_or("invalid JPEG XL frame size")?
                } else {
                    let mut rgba = RgbaImage::new(w, h);
                    if channels == 3 {
                        crate::image::expand_rgb(&samples, rgba.as_mut());
                    } else {
                        for (src, dst) in samples.chunks_exact(channels).zip(rgba.pixels_mut()) {
                            dst.0 = match channels {
                                1 => [src[0], src[0], src[0], 255],
                                2 => [src[0], src[0], src[0], src[1]],
                                _ => return Err("unsupported JPEG XL channels".into()),
                            };
                        }
                    }
                    rgba
                };
                let delay = self
                    .image
                    .image_header()
                    .metadata
                    .animation
                    .as_ref()
                    .map_or(Duration::ZERO, |a| {
                        Duration::from_secs_f64(
                            f64::from(render.duration()) * f64::from(a.tps_denominator)
                                / f64::from(a.tps_numerator.max(1)),
                        )
                    });
                Ok(Frame::from_parts(
                    rgba,
                    0,
                    0,
                    Delay::from_saturating_duration(delay),
                ))
            })()
            .map_err(error),
        )
    }
}

fn heif_result(error: heif::heif_error) -> Result<()> {
    if error.code == heif::heif_error_code_heif_error_Ok {
        Ok(())
    } else {
        // Libheif owns the error string and guarantees it is non-null.
        Err(unsafe { CStr::from_ptr(error.message) }
            .to_string_lossy()
            .into_owned()
            .into())
    }
}

macro_rules! heif_handle {
    ($name:ident, $type:ident, $free:ident) => {
        struct $name(NonNull<heif::$type>);
        impl Drop for $name {
            fn drop(&mut self) {
                unsafe { heif::$free(self.0.as_ptr()) }
            }
        }
    };
}
heif_handle!(HeifContext, heif_context, heif_context_free);
heif_handle!(HeifTrack, heif_track, heif_track_release);
heif_handle!(HeifImage, heif_image, heif_image_release);
heif_handle!(
    HeifOptions,
    heif_decoding_options,
    heif_decoding_options_free
);

struct HeifPoster<'a> {
    handle: NonNull<heif::heif_image_handle>,
    context: &'a HeifContext,
}
impl Drop for HeifPoster<'_> {
    fn drop(&mut self) {
        unsafe { heif::heif_image_handle_release(self.handle.as_ptr()) }
    }
}

fn fd_path(file: &File) -> String {
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

impl HeifContext {
    fn open(file: &File) -> Result<Self> {
        static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
        INIT.get_or_init(|| {
            heif_result(unsafe { heif::heif_init(null_mut()) }).map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(String::as_str)?;
        let context = Self(
            NonNull::new(unsafe { heif::heif_context_alloc() })
                .ok_or("could not allocate HEIF context")?,
        );
        let path = CString::new(fd_path(file))?;
        // The file reader opens and owns a separate descriptor for the context.
        heif_result(unsafe {
            heif::heif_context_read_from_file(context.0.as_ptr(), path.as_ptr(), null())
        })?;
        Ok(context)
    }

    fn has_sequence(&self) -> bool {
        unsafe { heif::heif_context_has_sequence(self.0.as_ptr()) != 0 }
    }

    fn poster(&self) -> Result<HeifPoster<'_>> {
        let mut handle = null_mut();
        heif_result(unsafe {
            heif::heif_context_get_primary_image_handle(self.0.as_ptr(), &mut handle)
        })?;
        Ok(HeifPoster {
            handle: NonNull::new(handle).ok_or("HEIF has no primary image")?,
            context: self,
        })
    }
}

fn heif_decode(
    passthrough: bool,
    decode: impl FnOnce(
        *mut *mut heif::heif_image,
        heif::heif_colorspace,
        heif::heif_chroma,
        *const heif::heif_decoding_options,
    ) -> heif::heif_error,
) -> Result<Option<HeifImage>> {
    let mut options = HeifOptions(
        NonNull::new(unsafe { heif::heif_decoding_options_alloc() })
            .ok_or("could not allocate HEIF decode options")?,
    );
    unsafe { options.0.as_mut().output_image_nclx_profile_passthrough = u8::from(passthrough) };
    let mut output = null_mut();
    let status = decode(
        &mut output,
        heif::heif_colorspace_heif_colorspace_RGB,
        heif::heif_chroma_heif_chroma_interleaved_RGBA,
        options.0.as_ptr(),
    );
    if status.code == heif::heif_error_code_heif_error_End_of_sequence {
        return Ok(None);
    }
    heif_result(status)?;
    Ok(Some(HeifImage(
        NonNull::new(output).ok_or("missing decoded HEIF image")?,
    )))
}

fn heif_icc(
    size: usize,
    read: impl FnOnce(*mut c_void) -> heif::heif_error,
) -> Result<Option<Vec<u8>>> {
    if size == 0 {
        return Ok(None);
    }
    let mut data = vec![0; size];
    heif_result(read(data.as_mut_ptr().cast()))?;
    Ok(Some(data))
}

type Nclx = (u32, u32);
fn heif_nclx(
    read: impl FnOnce(*mut *mut heif::heif_color_profile_nclx) -> heif::heif_error,
) -> Option<Nclx> {
    let mut profile = null_mut();
    heif_result(read(&mut profile)).ok()?;
    let profile = NonNull::new(profile)?;
    unsafe {
        let p = profile.as_ref();
        let nclx = (p.color_primaries, p.transfer_characteristics);
        heif::heif_nclx_color_profile_free(profile.as_ptr());
        Some(nclx)
    }
}

impl HeifPoster<'_> {
    fn size(&self) -> (u32, u32) {
        let handle = self.handle.as_ptr();
        unsafe {
            (
                heif::heif_image_handle_get_width(handle).max(0) as u32,
                heif::heif_image_handle_get_height(handle).max(0) as u32,
            )
        }
    }

    fn icc(&self) -> Result<Option<Vec<u8>>> {
        let handle = self.handle.as_ptr();
        unsafe {
            heif_icc(
                heif::heif_image_handle_get_raw_color_profile_size(handle),
                |out| heif::heif_image_handle_get_raw_color_profile(handle, out),
            )
        }
    }
}

struct HeifFrames {
    // The track must be released before its context.
    track: HeifTrack,
    context: HeifContext,
    timescale: u32,
    premultiplied: bool,
    ended: bool,
    profile: Option<Vec<u8>>,
    nclx: Option<Nclx>,
    color: Option<crate::color::Transform>,
}
impl HeifFrames {
    fn new(context: HeifContext) -> Result<Self> {
        let track = HeifTrack(
            NonNull::new(unsafe { heif::heif_context_get_track(context.0.as_ptr(), 0) })
                .ok_or("HEIF sequence has no visual track")?,
        );
        // Libheif does not carry the prem track reference onto decoded images.
        let premultiplied = unsafe {
            heif::heif_track_get_number_of_track_reference_of_type(
                track.0.as_ptr(),
                u32::from_be_bytes(*b"prem"),
            ) != 0
        };
        let timescale = unsafe { heif::heif_track_get_timescale(track.0.as_ptr()) };
        if timescale == 0 {
            return Err("invalid HEIF sequence timescale".into());
        }
        Ok(Self {
            track,
            context,
            timescale,
            premultiplied,
            ended: false,
            profile: None,
            nclx: None,
            color: None,
        })
    }
}
impl Iterator for HeifFrames {
    type Item = image::ImageResult<Frame>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        let track = self.track.0.as_ptr();
        let image = match heif_decode(true, |out, space, chroma, opts| unsafe {
            heif::heif_track_decode_next_image(track, out, space, chroma, opts)
        }) {
            Ok(Some(image)) => image,
            other => {
                self.ended = true;
                return other.err().map(|e| Err(error(e)));
            }
        };
        Some(
            (|| -> Result<Frame> {
                let ptr = image.0.as_ptr();
                let duration = Duration::from_secs_f64(
                    f64::from(unsafe { heif::heif_image_get_duration(ptr) })
                        / f64::from(self.timescale),
                );
                let profile = unsafe {
                    heif_icc(heif::heif_image_get_raw_color_profile_size(ptr), |out| {
                        heif::heif_image_get_raw_color_profile(ptr, out)
                    })?
                };
                let nclx =
                    heif_nclx(|out| unsafe { heif::heif_image_get_nclx_color_profile(ptr, out) });
                if profile != self.profile || nclx != self.nclx {
                    self.color = heif_color(profile.as_deref(), nclx)?;
                    self.profile = profile;
                    self.nclx = nclx;
                }
                let mut rgba = heif_rgba(&image, self.premultiplied)?;
                let width = rgba.width() as usize;
                crate::color::apply_rows(self.color.as_mut(), &mut rgba, width, false)?;
                Ok(Frame::from_parts(
                    rgba,
                    0,
                    0,
                    Delay::from_saturating_duration(duration),
                ))
            })()
            .map_err(error),
        )
    }
}

fn heif_color(
    profile: Option<&[u8]>,
    nclx: Option<Nclx>,
) -> Result<Option<crate::color::Transform>> {
    if profile.is_some() {
        return crate::color::Transform::new(profile);
    }
    let Some((primaries, transfer)) = nclx else {
        return Ok(None);
    };
    let primaries = match u8::try_from(primaries).unwrap_or(0) {
        2 => 1,
        n => n,
    };
    let transfer = match u8::try_from(transfer).unwrap_or(0) {
        2 => 13,
        n => n,
    };
    let (Ok(primaries), Ok(transfer)) = (
        moxcms::CicpColorPrimaries::try_from(primaries),
        moxcms::TransferCharacteristics::try_from(transfer),
    ) else {
        return Ok(None);
    };
    if moxcms::ColorPrimaries::try_from(primaries).is_err()
        || moxcms::ToneReprCurve::try_from(transfer).is_err()
    {
        return Ok(None);
    }
    crate::color::Transform::profile(moxcms::ColorProfile::new_from_cicp(moxcms::CicpProfile {
        color_primaries: primaries,
        transfer_characteristics: transfer,
        // The decoder already applied the YCbCr matrix and range to produce RGB.
        matrix_coefficients: moxcms::MatrixCoefficients::Identity,
        full_range: true,
    }))
    .map(Some)
}
const HEIF_RGBA: heif::heif_channel = heif::heif_channel_heif_channel_interleaved;

impl HeifImage {
    fn size(&self) -> Result<(u32, u32)> {
        let image = self.0.as_ptr();
        let width = unsafe { heif::heif_image_get_width(image, HEIF_RGBA) }.try_into()?;
        let height = unsafe { heif::heif_image_get_height(image, HEIF_RGBA) }.try_into()?;
        if width == 0 || height == 0 {
            return Err("invalid HEIF dimensions".into());
        }
        Ok((width, height))
    }
}

fn heif_rgba(image: &HeifImage, premultiplied: bool) -> Result<RgbaImage> {
    let (width, height) = image.size()?;
    let mut rgba = RgbaImage::new(width, height);
    heif_copy(image, rgba.as_mut(), premultiplied)?;
    Ok(rgba)
}

fn heif_copy(image: &HeifImage, rgba: &mut [u8], premultiplied: bool) -> Result<()> {
    let (width, height) = image.size()?;
    let ptr = image.0.as_ptr();
    let row = width as usize * 4;
    let mut stride = 0;
    let plane = unsafe { heif::heif_image_get_plane_readonly2(ptr, HEIF_RGBA, &mut stride) };
    if plane.is_null()
        || stride < row
        || unsafe { heif::heif_image_get_bits_per_pixel(ptr, HEIF_RGBA) } != 32
        || row.checked_mul(height as usize) != Some(rgba.len())
    {
        return Err("invalid HEIF RGBA layout".into());
    }
    let len = stride
        .checked_mul(height as usize)
        .filter(|&len| len <= isize::MAX as usize)
        .ok_or("HEIF plane is too large")?;
    // The plane remains valid while the decoded image is alive.
    let data = unsafe { std::slice::from_raw_parts(plane, len) };
    for (src, dst) in data.chunks_exact(stride).zip(rgba.chunks_exact_mut(row)) {
        dst.copy_from_slice(&src[..row]);
    }
    if premultiplied || unsafe { heif::heif_image_is_premultiplied_alpha(ptr) != 0 } {
        fast_image_resize::MulDiv::default().divide_alpha_inplace(
            &mut fast_image_resize::images::Image::from_slice_u8(
                width,
                height,
                rgba,
                fast_image_resize::PixelType::U8x4,
            )?,
        )?;
    }
    Ok(())
}
fn avif_preview(
    file: &File,
    poster: &HeifPoster<'_>,
    profile: Option<&[u8]>,
    limit: usize,
) -> Result<Option<Pixels>> {
    let handle = poster.handle.as_ptr();
    let nclx =
        heif_nclx(|out| unsafe { heif::heif_image_handle_get_nclx_color_profile(handle, out) });
    let sdr = nclx.is_none_or(|(primaries, transfer)| {
        matches!(primaries, 1 | 2 | 9 | 12) && matches!(transfer, 1 | 2 | 6 | 13 | 14 | 15)
    });
    if unsafe { heif::heif_image_handle_is_premultiplied_alpha(handle) != 0 }
        || (profile.is_none() && !sdr)
    {
        return Ok(None);
    }
    // The still decoder does not apply container crops, rotations or mirrors.
    let transformed = unsafe {
        heif::heif_item_get_transformation_properties(
            poster.context.0.as_ptr(),
            heif::heif_image_handle_get_item_id(handle),
            null_mut(),
            0,
        )
    };
    if transformed != 0 {
        return Ok(None);
    }
    let color = heif_color(profile, nclx)?;
    let reader = File::open(fd_path(file))?;
    let decoder = crate::image::decoder(
        BufReader::new(reader),
        Some(image::ImageFormat::Avif),
        limit,
    )?;
    let image = crate::color::convert_with(image::DynamicImage::from_decoder(decoder)?, color)?;
    Ok(Some(Pixels::raster(image)?))
}

pub fn preview(file: File, kind: Kind, limit: usize) -> Result<(Pixels, bool)> {
    let (first, animated) = match kind {
        Kind::Jxl => {
            let mut frames = JxlFrames::new(file, limit)?;
            let animated = frames.image.image_header().metadata.animation.is_some();
            (frames.next(), animated)
        }
        Kind::Heif | Kind::Avif => {
            let context = HeifContext::open(&file)?;
            if !context.has_sequence() {
                let poster = context.poster()?;
                let (width, height) = poster.size();
                if u64::from(width) * u64::from(height) > limit as u64 / 4 {
                    return Err("image exceeds the decoded memory budget".into());
                }
                let profile = poster.icc()?;
                if matches!(kind, Kind::Avif)
                    && let Some(pixels) = avif_preview(&file, &poster, profile.as_deref(), limit)?
                {
                    return Ok((pixels, false));
                }
                let decoded = heif_decode(profile.is_some(), |out, space, chroma, opts| unsafe {
                    heif::heif_decode_image(poster.handle.as_ptr(), out, space, chroma, opts)
                })?
                .ok_or("missing decoded HEIF image")?;
                drop(poster);
                drop(context);
                let (width, height) = decoded.size()?;
                let mut pixels = Pixels::new(width.try_into()?, height.try_into()?)?;
                heif_copy(&decoded, &mut pixels.mapping, false)?;
                drop(decoded);
                pixels.apply_color(crate::color::Transform::new(profile.as_deref())?, false)?;
                pixels.premultiply()?;
                return Ok((pixels, false));
            }
            let mut frames = HeifFrames::new(context)?;
            let (mut width, mut height) = (0_u16, 0_u16);
            heif_result(unsafe {
                heif::heif_track_get_image_resolution(
                    frames.track.0.as_ptr(),
                    &mut width,
                    &mut height,
                )
            })?;
            if u64::from(width) * u64::from(height) > limit as u64 / 4 {
                return Err("image exceeds the decoded memory budget".into());
            }
            // Playback is opt-in. Prefer a matching primary poster without starting the sequence decoder.
            if matches!(kind, Kind::Avif)
                && let Ok(poster) = frames.context.poster()
                && poster.size() == (u32::from(width), u32::from(height))
                && let Some(pixels) = avif_preview(&file, &poster, poster.icc()?.as_deref(), limit)?
            {
                return Ok((pixels, true));
            }
            (frames.next(), true)
        }
    };
    let frame = first.ok_or("image has no displayable frames")??;
    Ok((
        Pixels::raster(image::DynamicImage::ImageRgba8(frame.into_buffer()))?,
        animated,
    ))
}

pub fn frames(mut file: File) -> Result<(Frames<'static>, LoopCount)> {
    match kind(&mut file)?.ok_or("image is not animated")? {
        Kind::Jxl => {
            let frames = JxlFrames::new(file, usize::MAX)?;
            let loops = frames.loops();
            Ok((Frames::new(Box::new(frames)), loops))
        }
        Kind::Heif | Kind::Avif => {
            let context = HeifContext::open(&file)?;
            if !context.has_sequence() {
                return Err("image has no sequence".into());
            }
            let frames = HeifFrames::new(context)?;
            Ok((
                Frames::new(Box::new(frames)),
                LoopCount::Finite(NonZeroU32::new(1).unwrap()),
            ))
        }
    }
}
