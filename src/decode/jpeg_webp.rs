use crate::codec_ffi::{
    cmsColorSpaceSignature_cmsSigCmykData as CMYK, cmsColorSpaceSignature_cmsSigGrayData as GRAY,
    cmsColorSpaceSignature_cmsSigRgbData as RGB,
};
#[cfg(panic = "abort")]
compile_error!("JPEG error handling requires panic unwinding");

use crate::{
    Result,
    codec_ffi::{self, *},
    color::Profile,
    overview::Rasterizer,
    shm::Pixels,
};
use image::{ImageDecoder, ImageFormat, ImageReader};
use std::{
    ffi::c_void,
    io::{BufRead, Cursor, Read, Seek},
    mem::{MaybeUninit, size_of, transmute},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    ptr::{NonNull, null_mut},
};

impl Pixels {
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
                decode_jpeg(&bytes, expected, (1, 1), rect, size, cancelled)
            } else {
                Self::webp(&bytes, rect, size, true, header)
            }
        })
    }
    pub(super) fn native(
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
                decode_jpeg(
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
        let mut config = std::mem::MaybeUninit::<codec_ffi::WebPDecoderConfig>::uninit();
        let status = unsafe {
            if codec_ffi::WebPInitDecoderConfigInternal(
                config.as_mut_ptr(),
                codec_ffi::WEBP_DECODER_ABI_VERSION as i32,
            ) == 0
            {
                return Err("incompatible WebP decoder".into());
            }
            let mut config = config.assume_init();
            config.output.colorspace = codec_ffi::WEBP_CSP_MODE_MODE_RGBA;
            config.output.is_external_memory = 1;
            config.output.u.RGBA = codec_ffi::WebPRGBABuffer {
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
            let status = codec_ffi::WebPDecode(bytes.as_ptr(), bytes.len(), &mut config);
            codec_ffi::WebPFreeDecBuffer(&mut config.output);
            status
        };
        if status != codec_ffi::VP8StatusCode_VP8_STATUS_OK {
            return Err("cannot decode WebP".into());
        }
        pixels.apply_color(
            crate::color::Transform::new(header.icc_profile()?.as_deref())?,
            false,
        )?;
        pixels.premultiply()?;
        Ok(pixels)
    }
}

struct Jpeg(jpeg_decompress_struct);
impl Drop for Jpeg {
    fn drop(&mut self) {
        unsafe { jpeg_destroy_decompress(&mut self.0) };
    }
}

handle! { Transform(c_void) = cmsDeleteTransform; }

struct DecodeFailed;
unsafe extern "C-unwind" fn fail(_: j_common_ptr) {
    resume_unwind(Box::new(DecodeFailed));
}

unsafe extern "C-unwind" fn progress(jpeg: j_common_ptr) {
    // Only this decode uses the progress manager and the callback lives through the call.
    let cancelled = unsafe { &*(*jpeg).client_data.cast::<&dyn Fn() -> bool>() };
    if cancelled() {
        resume_unwind(Box::new(DecodeFailed));
    }
}

fn color(jpeg: &mut jpeg_decompress_struct, cmyk: bool) -> Option<Transform> {
    let (mut bytes, mut len) = (null_mut(), 0);
    if unsafe { jpeg_read_icc_profile(jpeg, &mut bytes, &mut len) } == 0 {
        return None;
    }
    let src = unsafe { NonNull::new(cmsOpenProfileFromMem(bytes.cast(), len)).map(Profile) };
    unsafe { libc::free(bytes.cast()) };
    let src = src?;
    let dst = Profile::srgb().ok()?;
    let format = match unsafe { cmsGetColorSpace(src.0.as_ptr()) } {
        CMYK if cmyk => PerchCmsFormat_CMYK8,
        RGB if !cmyk => PerchCmsFormat_RGBA8,
        GRAY if jpeg.jpeg_color_space == J_COLOR_SPACE_JCS_GRAYSCALE => PerchCmsFormat_GRAY8,
        _ => return None,
    };
    let transform = Transform(NonNull::new(unsafe {
        cmsCreateTransform(
            src.0.as_ptr(),
            format,
            dst.0.as_ptr(),
            PerchCmsFormat_RGBA8,
            INTENT_RELATIVE_COLORIMETRIC,
            0,
        )
    })?);
    if format == PerchCmsFormat_GRAY8 {
        jpeg.out_color_space = J_COLOR_SPACE_JCS_GRAYSCALE;
    }
    Some(transform)
}

fn decode_jpeg(
    bytes: &[u8],
    expected: (u32, u32),
    scale: (u32, u32),
    rect: (u32, u32, u32, u32),
    size: (u32, u32),
    cancelled: &dyn Fn() -> bool,
) -> Result<Pixels> {
    let mut error = MaybeUninit::<jpeg_error_mgr>::uninit();
    // Before creation libjpeg requires a zeroed struct with an initialized error manager.
    let mut jpeg = Jpeg(unsafe { std::mem::zeroed() });
    jpeg.0.err = unsafe { jpeg_std_error(error.as_mut_ptr()) };
    // The header's callbacks have C function pointer types. Libjpeg is built with
    // unwind support and calls these only while inside the catch_unwind below.
    unsafe {
        (*jpeg.0.err).error_exit = Some(transmute::<
            unsafe extern "C-unwind" fn(j_common_ptr),
            unsafe extern "C" fn(j_common_ptr),
        >(fail));
    }
    let mut monitor = jpeg_progress_mgr {
        progress_monitor: Some(unsafe {
            transmute::<unsafe extern "C-unwind" fn(j_common_ptr), unsafe extern "C" fn(j_common_ptr)>(
                progress,
            )
        }),
        pass_counter: 0,
        pass_limit: 0,
        completed_passes: 0,
        total_passes: 0,
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        let jpeg = &mut jpeg.0;
        unsafe {
            jpeg_CreateDecompress(
                jpeg,
                JPEG_LIB_VERSION as i32,
                size_of::<jpeg_decompress_struct>(),
            );
            jpeg_mem_src(jpeg, bytes.as_ptr(), bytes.len() as _);
            jpeg_save_markers(jpeg, (JPEG_APP0 + 2) as i32, 0xffff);
            jpeg_read_header(jpeg, 1);
        }
        if (jpeg.image_width, jpeg.image_height) != expected {
            return Err("image changed while open".into());
        }
        (jpeg.scale_num, jpeg.scale_denom) = scale;
        let cmyk = matches!(
            jpeg.jpeg_color_space,
            J_COLOR_SPACE_JCS_CMYK | J_COLOR_SPACE_JCS_YCCK
        );
        jpeg.out_color_space = if cmyk {
            J_COLOR_SPACE_JCS_CMYK
        } else {
            J_COLOR_SPACE_JCS_EXT_RGBA
        };
        let transform = color(jpeg, cmyk);
        jpeg.client_data = (&cancelled as *const &dyn Fn() -> bool).cast_mut().cast();
        jpeg.progress = &mut monitor;
        unsafe { jpeg_start_decompress(jpeg) };
        let (left, top, right, bottom) = rect;
        if right > jpeg.output_width || bottom > jpeg.output_height {
            return Err("invalid JPEG crop".into());
        }
        let mut raster = Rasterizer::new(rect, size, true)?;
        let out = if rect == (0, 0, jpeg.output_width, jpeg.output_height) {
            raster.pixels_mut().map(|p| p.mapping.as_mut_ptr())
        } else {
            None
        };
        let (mut x, mut width) = (left, right - left);
        if out.is_none() {
            unsafe { jpeg_crop_scanline(jpeg, &mut x, &mut width) };
        }
        let stride = width as usize * 4;
        let mut row = vec![0; if out.is_some() { 0 } else { stride }];
        let mut raw = vec![
            0;
            if jpeg.out_color_space == J_COLOR_SPACE_JCS_EXT_RGBA {
                0
            } else {
                stride
            }
        ];
        while jpeg.output_scanline < top {
            let n = (top - jpeg.output_scanline).min(128);
            if unsafe { jpeg_skip_scanlines(jpeg, n) } != n || cancelled() {
                return Err("cannot skip JPEG rows".into());
            }
        }
        while jpeg.output_scanline < bottom {
            let y = jpeg.output_scanline;
            let dst = if let Some(out) = out {
                unsafe { std::slice::from_raw_parts_mut(out.add(y as usize * stride), stride) }
            } else {
                &mut row
            };
            let mut src = if raw.is_empty() {
                dst.as_mut_ptr()
            } else {
                raw.as_mut_ptr()
            };
            if unsafe { jpeg_read_scanlines(jpeg, &mut src, 1) } != 1 {
                return Err("cannot read JPEG row".into());
            }
            if let Some(transform) = &transform {
                if cmyk && jpeg.saw_Adobe_marker != 0 {
                    for value in &mut raw {
                        *value = 255 - *value;
                    }
                }
                unsafe {
                    cmsDoTransform(
                        transform.0.as_ptr(),
                        src.cast(),
                        dst.as_mut_ptr().cast(),
                        width,
                    )
                };
                for pixel in dst.as_chunks_mut::<4>().0 {
                    pixel[3] = 255;
                }
            } else if cmyk {
                for (dst, src) in dst
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(raw.as_chunks::<4>().0)
                {
                    for c in 0..3 {
                        let (v, k) = if jpeg.saw_Adobe_marker != 0 {
                            (src[c], src[3])
                        } else {
                            (255 - src[c], 255 - src[3])
                        };
                        dst[c] = ((u16::from(v) * u16::from(k) + 127) / 255) as u8;
                    }
                    dst[3] = 255;
                }
            }
            if cancelled() {
                return Err("image loading cancelled".into());
            }
            if out.is_none() {
                raster.row(x, y, dst)?;
            }
        }
        let mut pixels = raster.finish()?;
        pixels.original_size = expected;
        Ok(pixels)
    }));
    match result {
        Ok(result) => result,
        Err(e) if e.is::<DecodeFailed>() => Err("cannot decode JPEG region".into()),
        Err(e) => resume_unwind(e),
    }
}

pub fn webp_info(bytes: &[u8]) -> Result<((u32, u32), bool)> {
    let mut features = std::mem::MaybeUninit::<codec_ffi::WebPBitstreamFeatures>::uninit();
    let features = unsafe {
        if codec_ffi::WebPGetFeaturesInternal(
            bytes.as_ptr(),
            bytes.len(),
            features.as_mut_ptr(),
            codec_ffi::WEBP_DECODER_ABI_VERSION as i32,
        ) != codec_ffi::VP8StatusCode_VP8_STATUS_OK
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
