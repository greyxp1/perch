use super::{CompressOptions, Raster};
use crate::{
    Result,
    decode::heif_avif::{
        HeifContext, HeifHandle, HeifImage, HeifPoster, cicp_profile, heif_decode, heif_nclx,
        heif_result, heif_rgba,
    },
};
use image::metadata::Orientation;
use libheif_sys as heif;
use std::{
    ffi::{CStr, c_void},
    ptr::{NonNull, null, null_mut},
    slice,
};

handle! {
    Encoder(heif::heif_encoder) = heif::heif_encoder_release;
    Options(heif::heif_encoding_options) = heif::heif_encoding_options_free;
    Nclx(heif::heif_color_profile_nclx) = heif::heif_nclx_color_profile_free;
}

/// Decodes the primary image, or the first frame of a sequence without one.
pub fn load(bytes: &[u8]) -> Result<Raster> {
    let context = read(bytes)?;
    match context.poster() {
        Ok(poster) => raster(&poster),
        Err(_) if context.has_sequence() => {
            let rgba = crate::decode::heif_avif::sequence_frame(context)?;
            Ok(Raster {
                width: rgba.width(),
                height: rgba.height(),
                rgba: rgba.into_raw(),
                icc: None,
                exif: None,
                xmp: None,
            })
        }
        Err(e) => Err(e),
    }
}

/// Re-encodes lossless stills losslessly and other stills only for lossy requests.
pub fn compress(bytes: &[u8], options: &CompressOptions) -> Result<Vec<u8>> {
    let context = read(bytes)?;
    if context.has_sequence() {
        return Err("animated AVIF cannot be compressed without losing frames".into());
    }
    let poster = context.poster()?;
    let handle = poster.handle.0.as_ptr();
    if unsafe { heif::heif_image_handle_get_luma_bits_per_pixel(handle) } > 8 {
        return Err("AVIF above 8 bits per channel cannot be compressed".into());
    }
    // Lossless AV1 stores RGB through the identity matrix, and every item, alpha included, is
    // coded at quantizer 0.
    let mut nclx = null_mut();
    let identity =
        heif_result(unsafe { heif::heif_image_handle_get_nclx_color_profile(handle, &mut nclx) })
            .is_ok()
            && NonNull::new(nclx).is_some_and(|nclx| {
                let nclx = Nclx(nclx);
                unsafe { nclx.0.as_ref() }.matrix_coefficients
                    == heif::heif_matrix_coefficients_heif_matrix_coefficients_RGB_GBR
            });
    let count = unsafe { heif::heif_image_handle_get_number_of_auxiliary_images(handle, 0) };
    let mut aux = vec![0; count.max(0) as usize];
    let count = unsafe {
        heif::heif_image_handle_get_list_of_auxiliary_image_IDs(handle, 0, aux.as_mut_ptr(), count)
    };
    let ctx = context.0.as_ptr();
    let lossless = identity
        && [unsafe { heif::heif_image_handle_get_item_id(handle) }]
            .iter()
            .chain(&aux[..count.max(0) as usize])
            .all(|&id| lossless_item(ctx, id));
    if !options.lossy && !lossless {
        return Err("lossy AVIF can only be compressed with --lossy".into());
    }
    let mut raster = raster(&poster)?;
    if !options.keep_metadata {
        (raster.exif, raster.xmp) = (None, None);
    }
    encode(&raster, options.quality, !options.lossy)
}

pub fn encode(raster: &Raster, quality: u8, lossless: bool) -> Result<Vec<u8>> {
    let context = HeifContext::new()?;
    let ctx = context.0.as_ptr();
    let mut encoder = null_mut();
    heif_result(unsafe {
        heif::heif_context_get_encoder_for_format(
            ctx,
            heif::heif_compression_format_heif_compression_AV1,
            &mut encoder,
        )
    })?;
    let encoder = Encoder(NonNull::new(encoder).ok_or("missing AV1 encoder")?);
    let enc = encoder.0.as_ptr();
    heif_result(unsafe {
        if lossless {
            heif::heif_encoder_set_lossless(enc, 1)
        } else {
            heif::heif_encoder_set_lossy_quality(enc, quality.into())
        }
    })?;
    if lossless {
        heif_result(unsafe {
            heif::heif_encoder_set_parameter_string(enc, c"chroma".as_ptr(), c"444".as_ptr())
        })?;
    }

    let image = image(raster)?;
    let nclx = Nclx(
        NonNull::new(unsafe { heif::heif_nclx_color_profile_alloc() })
            .ok_or("could not allocate AVIF color profile")?,
    );
    let n = unsafe { &mut *nclx.0.as_ptr() };
    // CICP 1 and 13 are sRGB, and 2 defers to the ICC profile.
    (n.color_primaries, n.transfer_characteristics) = match raster.icc {
        Some(_) => (2, 2),
        None => (1, 13),
    };
    // Lossless RGB must bypass YCbCr through the identity matrix.
    n.matrix_coefficients = if lossless { 0 } else { 6 };
    n.full_range_flag = 1;
    if let Some(icc) = &raster.icc {
        heif_result(unsafe {
            heif::heif_image_set_raw_color_profile(
                image.0.as_ptr(),
                c"prof".as_ptr(),
                icc.as_ptr().cast(),
                icc.len(),
            )
        })?;
    }
    let options = Options(
        NonNull::new(unsafe { heif::heif_encoding_options_alloc() })
            .ok_or("could not allocate AVIF options")?,
    );
    let o = unsafe { &mut *options.0.as_ptr() };
    o.output_nclx_profile = nclx.0.as_ptr();
    // Decoders need the matrix even when the ICC profile describes the color.
    o.save_two_colr_boxes_when_ICC_and_nclx_available = 1;

    let mut handle = null_mut();
    heif_result(unsafe {
        heif::heif_context_encode_image(ctx, image.0.as_ptr(), enc, options.0.as_ptr(), &mut handle)
    })?;
    let handle = HeifHandle(NonNull::new(handle).ok_or("missing encoded AVIF image")?);
    let h = handle.0.as_ptr();
    if let Some(exif) = &raster.exif {
        heif_result(unsafe {
            heif::heif_context_add_exif_metadata(
                ctx,
                h,
                exif.as_ptr().cast(),
                exif.len().try_into()?,
            )
        })?;
    }
    if let Some(xmp) = &raster.xmp {
        heif_result(unsafe {
            heif::heif_context_add_XMP_metadata(ctx, h, xmp.as_ptr().cast(), xmp.len().try_into()?)
        })?;
    }

    let mut out = Vec::<u8>::new();
    let mut writer = heif::heif_writer {
        writer_api_version: 1,
        write: Some(write),
    };
    heif_result(unsafe { heif::heif_context_write(ctx, &mut writer, (&raw mut out).cast()) })?;
    Ok(out)
}

unsafe extern "C" fn write(
    _: *mut heif::heif_context,
    data: *const c_void,
    size: usize,
    out: *mut c_void,
) -> heif::heif_error {
    // Libheif passes back the vector given to heif_context_write.
    unsafe { (*out.cast::<Vec<u8>>()).extend_from_slice(slice::from_raw_parts(data.cast(), size)) };
    heif::heif_error {
        code: heif::heif_error_code_heif_error_Ok,
        subcode: heif::heif_suberror_code_heif_suberror_Unspecified,
        message: c"".as_ptr(),
    }
}

fn read(bytes: &[u8]) -> Result<HeifContext> {
    let context = HeifContext::new()?;
    heif_result(unsafe {
        heif::heif_context_read_from_memory(
            context.0.as_ptr(),
            bytes.as_ptr().cast(),
            bytes.len(),
            null(),
        )
    })?;
    Ok(context)
}

fn lossless_item(ctx: *const heif::heif_context, id: heif::heif_item_id) -> bool {
    let (mut data, mut size) = (null_mut(), 0);
    if heif_result(unsafe {
        heif::heif_item_get_item_data(ctx, id, null_mut(), &mut data, &mut size)
    })
    .is_err()
        || data.is_null()
    {
        return false;
    }
    let lossless = av1_lossless(unsafe { slice::from_raw_parts(data, size) }).unwrap_or(false);
    unsafe { heif::heif_release_item_data(ctx, &mut data) };
    lossless
}

/// Reads whether an AV1 still is coded losslessly, following the AV1 specification sections 5.5
/// and 5.9. Only reduced still picture headers, which AVIF encoders write, are understood.
fn av1_lossless(mut data: &[u8]) -> Option<bool> {
    let mut sequence = None;
    while let [header, rest @ ..] = data {
        let rest = rest.get(usize::from(header >> 2 & 1)..)?;
        let (mut size, mut length) = (0, 0);
        if header & 2 == 0 {
            size = rest.len();
        } else {
            loop {
                let byte = *rest.get(length)?;
                size |= usize::from(byte & 0x7f).checked_shl(7 * length as u32)?;
                length += 1;
                if byte < 0x80 {
                    break;
                }
            }
        }
        let payload = rest.get(length..length.checked_add(size)?)?;
        data = &rest[length + size..];
        let mut bits = Bits(payload, 0);
        match header >> 3 & 15 {
            1 => sequence = Some(Sequence::parse(&mut bits)?),
            3 | 6 => return sequence?.lossless_frame(&mut bits),
            _ => {}
        }
    }
    None
}

#[derive(Clone, Copy)]
struct Sequence {
    width: u32,
    height: u32,
    superblock_128: bool,
    superres: bool,
    planes: u8,
    separate_uv_delta_q: bool,
}

impl Sequence {
    fn parse(b: &mut Bits) -> Option<Self> {
        let profile = b.f(3)?;
        // Still picture and reduced still picture header
        if b.f(2)? != 0b11 {
            return None;
        }
        b.f(5)?;
        let (width_bits, height_bits) = (b.f(4)? + 1, b.f(4)? + 1);
        let (width, height) = (b.f(width_bits)? + 1, b.f(height_bits)? + 1);
        let superblock_128 = b.flag()?;
        // Filter intra and intra edge filter
        b.f(2)?;
        let superres = b.flag()?;
        // CDEF and loop restoration
        b.f(2)?;
        let twelve_bit = b.flag()? && profile == 2 && b.flag()?;
        let mono = profile != 1 && b.flag()?;
        let (primaries, transfer, matrix) = if b.flag()? {
            (b.f(8)?, b.f(8)?, b.f(8)?)
        } else {
            (2, 2, 2)
        };
        let separate_uv_delta_q = if mono {
            b.f(1)?;
            false
        } else {
            if (primaries, transfer, matrix) != (1, 13, 0) {
                b.f(1)?;
                let subsampled = match profile {
                    0 => true,
                    1 => false,
                    _ => twelve_bit && b.flag()? && b.flag()?,
                };
                if subsampled {
                    b.f(2)?;
                }
            }
            b.flag()?
        };
        // Film grain is added after decoding.
        if b.flag()? {
            return None;
        }
        Some(Self {
            width,
            height,
            superblock_128,
            superres,
            planes: if mono { 1 } else { 3 },
            separate_uv_delta_q,
        })
    }

    /// Reads a key frame header up to the segmentation flag.
    fn lossless_frame(self, b: &mut Bits) -> Option<bool> {
        // CDF update
        b.f(1)?;
        let screen_content = b.flag()?;
        if screen_content {
            b.f(1)?;
        }
        if self.superres && b.flag()? {
            return Some(false);
        }
        if b.flag()? {
            b.f(32)?;
        }
        if screen_content {
            b.f(1)?;
        }

        let shift = if self.superblock_128 { 5 } else { 4 };
        let mi = |pixels: u32| 2 * pixels.div_ceil(8);
        let sb = |pixels| (mi(pixels) + (1 << shift) - 1) >> shift;
        let (cols, rows) = (sb(self.width), sb(self.height));
        let max_width = 4096 >> (shift + 2);
        let max_area = (4096 * 2304) >> (2 * (shift + 2));
        let min_cols_log2 = tile_log2(max_width, cols);
        let max_cols_log2 = tile_log2(1, cols.min(64));
        let max_rows_log2 = tile_log2(1, rows.min(64));
        let min_log2 = min_cols_log2.max(tile_log2(max_area, rows * cols));
        let (cols_log2, rows_log2) = if b.flag()? {
            let mut cols_log2 = min_cols_log2;
            while cols_log2 < max_cols_log2 && b.flag()? {
                cols_log2 += 1;
            }
            let mut rows_log2 = min_log2.saturating_sub(cols_log2);
            while rows_log2 < max_rows_log2 && b.flag()? {
                rows_log2 += 1;
            }
            (cols_log2, rows_log2)
        } else {
            let (mut start, mut tiles, mut widest) = (0, 0, 0);
            while start < cols {
                let size = b.ns((cols - start).min(max_width))? + 1;
                widest = widest.max(size);
                start += size;
                tiles += 1;
            }
            let cols_log2 = tile_log2(1, tiles);
            let area = if min_log2 > 0 {
                (rows * cols) >> (min_log2 + 1)
            } else {
                rows * cols
            };
            let max_height = (area / widest).max(1);
            (start, tiles) = (0, 0);
            while start < rows {
                start += b.ns((rows - start).min(max_height))? + 1;
                tiles += 1;
            }
            (cols_log2, tile_log2(1, tiles))
        };
        if cols_log2 + rows_log2 > 0 {
            b.f(cols_log2 + rows_log2 + 2)?;
        }

        let base_q = b.f(8)?;
        let deltas = match self.planes {
            1 => 1,
            _ if self.separate_uv_delta_q && b.flag()? => 5,
            _ => 3,
        };
        let mut lossless = base_q == 0;
        for _ in 0..deltas {
            if b.flag()? {
                lossless &= b.f(7)? == 0;
            }
        }
        // Quantizer matrices do not apply to lossless frames.
        if b.flag()? {
            b.f(if self.separate_uv_delta_q { 12 } else { 8 })?;
        }
        // Segments could raise the quantizer.
        Some(lossless && !b.flag()?)
    }
}

fn tile_log2(size: u32, target: u32) -> u32 {
    let mut k = 0;
    while size << k < target {
        k += 1;
    }
    k
}

struct Bits<'a>(&'a [u8], usize);

impl Bits<'_> {
    fn f(&mut self, n: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..n {
            let byte = self.0.get(self.1 / 8)?;
            value = value << 1 | u32::from(byte >> (7 - self.1 % 8) & 1);
            self.1 += 1;
        }
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.f(1)? == 1)
    }

    fn ns(&mut self, n: u32) -> Option<u32> {
        let w = n.ilog2() + 1;
        let m = (1 << w) - n;
        let v = self.f(w - 1)?;
        Some(if v < m { v } else { (v << 1) - m + self.f(1)? })
    }
}

/// Copies the pixels into an interleaved image, leaving out opaque alpha.
fn image(raster: &Raster) -> Result<HeifImage> {
    let opaque = raster.opaque();
    let (width, height) = (raster.width.try_into()?, raster.height.try_into()?);
    let mut image = null_mut();
    heif_result(unsafe {
        heif::heif_image_create(
            width,
            height,
            heif::heif_colorspace_heif_colorspace_RGB,
            if opaque {
                heif::heif_chroma_heif_chroma_interleaved_RGB
            } else {
                heif::heif_chroma_heif_chroma_interleaved_RGBA
            },
            &mut image,
        )
    })?;
    let image = HeifImage(NonNull::new(image).ok_or("could not allocate AVIF image")?);
    let channel = heif::heif_channel_heif_channel_interleaved;
    heif_result(unsafe {
        heif::heif_image_add_plane(image.0.as_ptr(), channel, width, height, 8)
    })?;
    let mut stride = 0;
    let plane = unsafe { heif::heif_image_get_plane2(image.0.as_ptr(), channel, &mut stride) };
    let channels = if opaque { 3 } else { 4 };
    let row = raster.width as usize * channels;
    if plane.is_null() || stride < row {
        return Err("invalid AVIF image layout".into());
    }
    // Libheif allocates `height` rows of `stride` bytes.
    let plane = unsafe { slice::from_raw_parts_mut(plane, stride * raster.height as usize) };
    for (dst, src) in plane
        .chunks_mut(stride)
        .zip(raster.rgba.chunks_exact(raster.width as usize * 4))
    {
        if opaque {
            for (d, s) in dst[..row]
                .as_chunks_mut::<3>()
                .0
                .iter_mut()
                .zip(src.as_chunks::<4>().0)
            {
                d.copy_from_slice(&s[..3]);
            }
        } else {
            dst[..row].copy_from_slice(src);
        }
    }
    Ok(image)
}

fn raster(poster: &HeifPoster<'_>) -> Result<Raster> {
    let handle = poster.handle.0.as_ptr();
    let icc = poster.icc()?;
    let decoded = heif_decode(icc.is_some(), |out, space, chroma, opts| unsafe {
        heif::heif_decode_image(handle, out, space, chroma, opts)
    })?
    .ok_or("missing decoded HEIF image")?;
    let premultiplied = unsafe { heif::heif_image_handle_is_premultiplied_alpha(handle) != 0 };
    let rgba = heif_rgba(&decoded, premultiplied)?;
    let icc = match icc {
        Some(icc) => Some(icc),
        None => match heif_nclx(|out| unsafe {
            heif::heif_image_handle_get_nclx_color_profile(handle, out)
        }) {
            None | Some((1 | 2, 2 | 13)) => None,
            Some(nclx) => cicp_profile(nclx).map(|p| p.encode()).transpose()?,
        },
    };
    let exif = metadata(handle, c"Exif", None)?.and_then(|data| {
        let offset = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
        let mut tiff = data.get(offset.checked_add(4)?..)?.to_vec();
        // Libheif applied the container rotation, which supersedes the EXIF one.
        let _ = Orientation::remove_from_exif_chunk(&mut tiff);
        Some(tiff)
    });
    let mut raster = Raster {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
        icc,
        exif,
        xmp: metadata(handle, c"mime", Some(c"application/rdf+xml"))?,
    };
    raster.fit_profile()?;
    Ok(raster)
}

/// Returns the first metadata block of an item type and optional content type.
fn metadata(
    handle: *const heif::heif_image_handle,
    kind: &CStr,
    content: Option<&CStr>,
) -> Result<Option<Vec<u8>>> {
    let count =
        unsafe { heif::heif_image_handle_get_number_of_metadata_blocks(handle, kind.as_ptr()) };
    let mut ids = vec![0; count.max(0) as usize];
    let count = unsafe {
        heif::heif_image_handle_get_list_of_metadata_block_IDs(
            handle,
            kind.as_ptr(),
            ids.as_mut_ptr(),
            count,
        )
    };
    for &id in &ids[..count.max(0) as usize] {
        if let Some(content) = content
            && unsafe {
                CStr::from_ptr(heif::heif_image_handle_get_metadata_content_type(
                    handle, id,
                ))
            } != content
        {
            continue;
        }
        let mut data = vec![0; unsafe { heif::heif_image_handle_get_metadata_size(handle, id) }];
        heif_result(unsafe {
            heif::heif_image_handle_get_metadata(handle, id, data.as_mut_ptr().cast())
        })?;
        return Ok(Some(data));
    }
    Ok(None)
}
