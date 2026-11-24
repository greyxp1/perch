use super::{Kind, decoder, error, first_frame};
use crate::{Result, shm::Pixels};
use image::{Delay, Frame, Frames, RgbaImage, metadata::LoopCount};
use libheif_sys as heif;
use std::{
    ffi::{CStr, CString, c_void},
    fs::File,
    io::BufReader,
    num::NonZeroU32,
    ptr::{NonNull, null, null_mut},
    sync::OnceLock,
    time::Duration,
};

pub(crate) fn heif_result(error: heif::heif_error) -> Result<()> {
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

handle! {
    pub(crate) HeifContext(heif::heif_context) = heif::heif_context_free;
    HeifTrack(heif::heif_track) = heif::heif_track_release;
    pub(crate) HeifImage(heif::heif_image) = heif::heif_image_release;
    HeifOptions(heif::heif_decoding_options) = heif::heif_decoding_options_free;
    pub(crate) HeifHandle(heif::heif_image_handle) = heif::heif_image_handle_release;
}

pub(crate) struct HeifPoster<'a> {
    pub(crate) handle: HeifHandle,
    context: &'a HeifContext,
}

impl HeifContext {
    pub(crate) fn new() -> Result<Self> {
        static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
        INIT.get_or_init(|| {
            heif_result(unsafe { heif::heif_init(null_mut()) }).map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(String::as_str)?;
        Ok(Self(
            NonNull::new(unsafe { heif::heif_context_alloc() })
                .ok_or("could not allocate HEIF context")?,
        ))
    }

    fn open(file: &File) -> Result<Self> {
        let context = Self::new()?;
        let path = CString::new(crate::fd_path(file))?;
        // The file reader opens and owns a separate descriptor for the context.
        heif_result(unsafe {
            heif::heif_context_read_from_file(context.0.as_ptr(), path.as_ptr(), null())
        })?;
        Ok(context)
    }

    pub(crate) fn has_sequence(&self) -> bool {
        unsafe { heif::heif_context_has_sequence(self.0.as_ptr()) != 0 }
    }

    pub(crate) fn poster(&self) -> Result<HeifPoster<'_>> {
        let mut handle = null_mut();
        heif_result(unsafe {
            heif::heif_context_get_primary_image_handle(self.0.as_ptr(), &mut handle)
        })?;
        Ok(HeifPoster {
            handle: HeifHandle(NonNull::new(handle).ok_or("HEIF has no primary image")?),
            context: self,
        })
    }
}

pub(crate) fn heif_decode(
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

pub(crate) type Nclx = (u32, u32);
pub(crate) fn heif_nclx(
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
        let handle = self.handle.0.as_ptr();
        unsafe {
            (
                heif::heif_image_handle_get_width(handle).max(0) as u32,
                heif::heif_image_handle_get_height(handle).max(0) as u32,
            )
        }
    }

    pub(crate) fn icc(&self) -> Result<Option<Vec<u8>>> {
        let handle = self.handle.0.as_ptr();
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
    nclx.and_then(cicp_profile)
        .map(crate::color::Transform::profile)
        .transpose()
}

/// Describes decoded RGB with NCLX primaries and transfer, treating unspecified values as sRGB.
pub(crate) fn cicp_profile((primaries, transfer): Nclx) -> Option<moxcms::ColorProfile> {
    let primaries = match u8::try_from(primaries).unwrap_or(0) {
        2 => 1,
        n => n,
    };
    let transfer = match u8::try_from(transfer).unwrap_or(0) {
        2 => 13,
        n => n,
    };
    let primaries = moxcms::CicpColorPrimaries::try_from(primaries).ok()?;
    let transfer = moxcms::TransferCharacteristics::try_from(transfer).ok()?;
    if moxcms::ColorPrimaries::try_from(primaries).is_err()
        || moxcms::ToneReprCurve::try_from(transfer).is_err()
    {
        return None;
    }
    Some(moxcms::ColorProfile::new_from_cicp(moxcms::CicpProfile {
        color_primaries: primaries,
        transfer_characteristics: transfer,
        // The decoder already applied the YCbCr matrix and range to produce RGB.
        matrix_coefficients: moxcms::MatrixCoefficients::Identity,
        full_range: true,
    }))
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

pub(crate) fn heif_rgba(image: &HeifImage, premultiplied: bool) -> Result<RgbaImage> {
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
    let handle = poster.handle.0.as_ptr();
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
    let reader = File::open(crate::fd_path(file))?;
    let decoder = decoder(
        BufReader::new(reader),
        Some(image::ImageFormat::Avif),
        limit,
    )?;
    let image = crate::color::convert_with(image::DynamicImage::from_decoder(decoder)?, color)?;
    Ok(Some(Pixels::raster(image)?))
}

pub(super) fn preview(file: File, kind: Kind, limit: usize) -> Result<(Pixels, bool)> {
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
            heif::heif_decode_image(poster.handle.0.as_ptr(), out, space, chroma, opts)
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
    let frames = HeifFrames::new(context)?;
    let (mut width, mut height) = (0_u16, 0_u16);
    heif_result(unsafe {
        heif::heif_track_get_image_resolution(frames.track.0.as_ptr(), &mut width, &mut height)
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
    Ok((first_frame(frames)?, true))
}

pub(super) fn frames(file: File) -> Result<(Frames<'static>, LoopCount)> {
    let context = HeifContext::open(&file)?;
    if !context.has_sequence() {
        return Err("image has no sequence".into());
    }
    let frames = Frames::new(Box::new(HeifFrames::new(context)?));
    Ok((frames, LoopCount::Finite(NonZeroU32::MIN)))
}

/// Decodes the first frame of a sequence as sRGB.
pub(crate) fn sequence_frame(context: HeifContext) -> Result<RgbaImage> {
    let frame = HeifFrames::new(context)?
        .next()
        .ok_or("HEIF sequence has no frames")??;
    Ok(frame.into_buffer())
}
