use super::Raster;
use crate::{
    Result,
    codec_ffi::*,
    decode::{DecodeFailed, Jpeg, jpeg_errors},
};
use image::metadata::Orientation;
use std::{
    mem::{MaybeUninit, size_of, zeroed},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    slice,
};

const APP1: i32 = JPEG_APP0 as i32 + 1;

pub fn encode(raster: &Raster, quality: u8) -> Result<Vec<u8>> {
    compress(|jpeg| unsafe {
        jpeg.image_width = raster.width;
        jpeg.image_height = raster.height;
        jpeg.input_components = 4;
        jpeg.in_color_space = J_COLOR_SPACE_JCS_EXT_RGBA;
        jpeg_set_defaults(jpeg);
        jpeg_set_quality(jpeg, quality.into(), 1);
        jpeg.optimize_coding = 1;
        jpeg_start_compress(jpeg, 1);
        if let Some(exif) = &raster.exif {
            write_marker(jpeg, APP1, b"Exif\0\0", exif);
        }
        if let Some(xmp) = &raster.xmp {
            write_marker(jpeg, APP1, b"http://ns.adobe.com/xap/1.0/\0", xmp);
        }
        if let Some(icc) = &raster.icc {
            jpeg_write_icc_profile(jpeg, icc.as_ptr(), icc.len().try_into()?);
        }
        let stride = raster.width as usize * 4;
        while jpeg.next_scanline < jpeg.image_height {
            let mut row = raster.rgba[jpeg.next_scanline as usize * stride..]
                .as_ptr()
                .cast_mut();
            jpeg_write_scanlines(jpeg, &mut row, 1);
        }
        Ok(())
    })
}

/// Reads the markers and DCT coefficients of a JPEG and passes them to `then`.
fn read<T>(
    bytes: &[u8],
    then: impl FnOnce(&mut jpeg_decompress_struct, *mut jvirt_barray_ptr) -> Result<T>,
) -> Result<T> {
    let mut error = MaybeUninit::uninit();
    // Before creation libjpeg requires a zeroed struct with an initialized error manager.
    let mut source = Jpeg(unsafe { zeroed() });
    source.0.err = jpeg_errors(&mut error);
    guarded(|| unsafe {
        let source = &mut source.0;
        jpeg_CreateDecompress(
            source,
            JPEG_LIB_VERSION as i32,
            size_of::<jpeg_decompress_struct>(),
        );
        jpeg_mem_src(source, bytes.as_ptr(), bytes.len() as _);
        for marker in (JPEG_APP0..JPEG_APP0 + 16).chain([JPEG_COM]) {
            jpeg_save_markers(source, marker as i32, 0xffff);
        }
        jpeg_read_header(source, 1);
        let coefficients = jpeg_read_coefficients(source);
        // Truncated or corrupt scans only warn, and their reconstruction must not replace them.
        if (*source.err).num_warnings > 0 {
            return Err("the JPEG is truncated or corrupt".into());
        }
        then(source, coefficients)
    })
}

/// Rejects JPEGs that decoders would only partly reconstruct.
pub fn check(bytes: &[u8]) -> Result<()> {
    read(bytes, |_, _| Ok(()))
}

/// Rewrites the DCT coefficients with optimized Huffman tables, so pixels stay identical.
pub fn transcode(bytes: &[u8], keep_metadata: bool) -> Result<Vec<u8>> {
    read(bytes, |source, coefficients| unsafe {
        let progressive = source.progressive_mode != 0;
        let list = source.marker_list;
        let markers = move || {
            let mut marker = list;
            std::iter::from_fn(move || {
                let current = marker.as_ref()?;
                marker = current.next;
                let data = slice::from_raw_parts(current.data, current.data_length as usize);
                Some((i32::from(current.marker), data))
            })
        };
        let orientation = markers()
            .find_map(|(code, data)| (code == APP1).then(|| data.strip_prefix(b"Exif\0\0"))?)
            .and_then(Orientation::from_exif_chunk);
        compress(|target| {
            jpeg_copy_critical_parameters(source, target);
            target.optimize_coding = 1;
            if progressive {
                jpeg_simple_progression(target);
            }
            jpeg_write_coefficients(target, coefficients);
            if !keep_metadata && let Some(exif) = super::orientation_exif(orientation) {
                write_marker(target, APP1, b"Exif\0\0", &exif);
            }
            for (code, data) in markers() {
                let written = match code - JPEG_APP0 as i32 {
                    0 if data.starts_with(b"JFIF\0") => target.write_JFIF_header != 0,
                    14 if data.starts_with(b"Adobe") => target.write_Adobe_marker != 0,
                    _ => false,
                };
                let profile = code == JPEG_APP0 as i32 + 2 && data.starts_with(b"ICC_PROFILE\0");
                if !written && (keep_metadata || profile) {
                    write_marker(target, code, b"", data);
                }
            }
            Ok(())
        })
    })
}

/// Reports whether a JPEG frame has four components, which needs CMYK conversion.
pub fn cmyk(bytes: &[u8]) -> bool {
    let mut i = 2;
    while let Some(&[0xff, marker, high, low, ..]) = bytes.get(i..i + 10) {
        if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            return bytes[i + 9] == 4;
        }
        i += 2 + usize::from(u16::from_be_bytes([high, low]));
    }
    false
}

unsafe fn write_marker(jpeg: &mut jpeg_compress_struct, code: i32, prefix: &[u8], data: &[u8]) {
    // Segments longer than one marker cannot be written and are dropped.
    if prefix.len() + data.len() <= 65533 {
        let marker = [prefix, data].concat();
        unsafe { jpeg_write_marker(jpeg, code, marker.as_ptr(), marker.len() as u32) };
    }
}

fn guarded<T>(run: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(result) => result,
        Err(e) if e.is::<DecodeFailed>() => Err("cannot encode JPEG".into()),
        Err(e) => resume_unwind(e),
    }
}

struct Compress(jpeg_compress_struct);
impl Drop for Compress {
    fn drop(&mut self) {
        unsafe { jpeg_destroy_compress(&mut self.0) };
    }
}

/// Collects compressed bytes in the spare capacity of a vector.
#[repr(C)]
struct Destination {
    manager: jpeg_destination_mgr,
    out: Vec<u8>,
}
impl Destination {
    unsafe fn get<'a>(jpeg: j_compress_ptr) -> &'a mut Self {
        // The manager is the first field of the destination that `compress` installs.
        unsafe { &mut *(*jpeg).dest.cast::<Self>() }
    }
    fn spare(&mut self) {
        self.manager.next_output_byte = self.out.as_mut_ptr().wrapping_add(self.out.len());
        self.manager.free_in_buffer = self.out.capacity() - self.out.len();
    }
}
unsafe extern "C" fn init(jpeg: j_compress_ptr) {
    let destination = unsafe { Destination::get(jpeg) };
    destination.out.reserve(1 << 16);
    destination.spare();
}
unsafe extern "C" fn empty(jpeg: j_compress_ptr) -> boolean {
    let destination = unsafe { Destination::get(jpeg) };
    // Libjpeg filled the whole spare capacity before asking for more.
    unsafe { destination.out.set_len(destination.out.capacity()) };
    destination.out.reserve(destination.out.len());
    destination.spare();
    1
}
unsafe extern "C" fn term(jpeg: j_compress_ptr) {
    let destination = unsafe { Destination::get(jpeg) };
    let len = destination.out.capacity() - destination.manager.free_in_buffer;
    unsafe { destination.out.set_len(len) };
}

/// Creates a compressor, runs `setup` and finishes it into memory.
fn compress(setup: impl FnOnce(&mut jpeg_compress_struct) -> Result<()>) -> Result<Vec<u8>> {
    let mut destination = Destination {
        manager: jpeg_destination_mgr {
            next_output_byte: std::ptr::null_mut(),
            free_in_buffer: 0,
            init_destination: Some(init),
            empty_output_buffer: Some(empty),
            term_destination: Some(term),
        },
        out: Vec::new(),
    };
    let mut error = MaybeUninit::uninit();
    let mut jpeg = Compress(unsafe { zeroed() });
    jpeg.0.err = jpeg_errors(&mut error);
    guarded(|| {
        let jpeg = &mut jpeg.0;
        unsafe {
            jpeg_CreateCompress(
                jpeg,
                JPEG_LIB_VERSION as i32,
                size_of::<jpeg_compress_struct>(),
            );
        }
        jpeg.dest = (&raw mut destination).cast();
        setup(jpeg)?;
        unsafe { jpeg_finish_compress(jpeg) };
        Ok(())
    })?;
    drop(jpeg);
    Ok(destination.out)
}
