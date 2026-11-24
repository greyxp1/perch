use super::{CompressOptions, Raster};
use crate::{Result, codec_ffi::*};
use image::metadata::Orientation;
use std::{
    ffi::{CStr, c_void},
    mem::MaybeUninit,
    ptr::NonNull,
    slice,
};

pub fn encode(raster: &Raster, quality: u8, lossless: bool) -> Result<Vec<u8>> {
    let bitstream = bitstream(raster, quality, lossless)?;
    let chunks = [
        (c"ICCP", &raster.icc),
        (c"EXIF", &raster.exif),
        (c"XMP ", &raster.xmp),
    ];
    if chunks.iter().all(|(_, data)| data.is_none()) {
        return Ok(bitstream);
    }
    let mut mux = Mux::new(&bitstream)?;
    for (name, data) in chunks {
        if let Some(data) = data {
            mux.set(name, data)?;
        }
    }
    mux.assemble()
}

/// Re-encodes lossless and lossy requests, or strips metadata without touching other bitstreams.
pub fn compress(
    bytes: &[u8],
    options: &CompressOptions,
    decode: impl FnOnce() -> Result<Raster>,
) -> Result<Vec<u8>> {
    let features = crate::decode::webp_features(bytes)?;
    let still = features.has_animation == 0;
    if still && options.lossy {
        return encode(&decode()?, options.quality, false);
    }
    // Format 2 is a lossless bitstream, which re-encodes without changing pixels.
    if still && features.format == 2 {
        return encode(&decode()?, 100, true);
    }
    if options.keep_metadata {
        return Ok(bytes.to_vec());
    }
    let mut mux = Mux::new(bytes)?;
    let orientation = mux
        .chunk(c"EXIF")
        .map(|exif| exif.strip_prefix(b"Exif\0\0").unwrap_or(exif))
        .and_then(Orientation::from_exif_chunk);
    let exif = super::orientation_exif(orientation);
    mux.delete(c"EXIF");
    mux.delete(c"XMP ");
    if let Some(exif) = exif {
        mux.set(c"EXIF", &exif)?;
    }
    mux.assemble()
}

fn bitstream(raster: &Raster, quality: u8, lossless: bool) -> Result<Vec<u8>> {
    let mut config = MaybeUninit::uninit();
    let mut picture = Picture(unsafe { std::mem::zeroed() });
    let mut writer = Writer(unsafe { std::mem::zeroed() });
    unsafe {
        if WebPConfigInitInternal(
            config.as_mut_ptr(),
            WebPPreset_WEBP_PRESET_DEFAULT,
            75.0,
            WEBP_ENCODER_ABI_VERSION as i32,
        ) == 0
            || WebPPictureInitInternal(&mut picture.0, WEBP_ENCODER_ABI_VERSION as i32) == 0
        {
            return Err("incompatible WebP encoder".into());
        }
        WebPMemoryWriterInit(&mut writer.0);
    }
    let mut config = unsafe { config.assume_init() };
    if lossless {
        unsafe { WebPConfigLosslessPreset(&mut config, 6) };
        config.exact = i32::from(!raster.opaque());
    } else {
        config.quality = f32::from(quality);
        config.method = 6;
    }
    let picture = &mut picture.0;
    picture.use_argb = i32::from(lossless);
    picture.width = raster.width.try_into()?;
    picture.height = raster.height.try_into()?;
    picture.writer = Some(WebPMemoryWrite);
    picture.custom_ptr = (&raw mut writer.0).cast();
    let encoded = unsafe {
        WebPValidateConfig(&config) != 0
            && WebPPictureImportRGBA(picture, raster.rgba.as_ptr(), picture.width * 4) != 0
            && WebPEncode(&config, picture) != 0
    };
    if !encoded {
        return Err(format!("cannot encode WebP (error {})", picture.error_code).into());
    }
    Ok(unsafe { slice::from_raw_parts(writer.0.mem, writer.0.size) }.to_vec())
}

struct Picture(WebPPicture);
impl Drop for Picture {
    fn drop(&mut self) {
        unsafe { WebPPictureFree(&mut self.0) };
    }
}

struct Writer(WebPMemoryWriter);
impl Drop for Writer {
    fn drop(&mut self) {
        unsafe { WebPMemoryWriterClear(&mut self.0) };
    }
}

handle! { Mux(WebPMux) = WebPMuxDelete; }
impl Mux {
    fn new(bytes: &[u8]) -> Result<Self> {
        let data = WebPData {
            bytes: bytes.as_ptr(),
            size: bytes.len(),
        };
        NonNull::new(unsafe { WebPMuxCreateInternal(&data, 1, WEBP_MUX_ABI_VERSION as i32) })
            .map(Self)
            .ok_or_else(|| "invalid WebP container".into())
    }
    fn chunk(&self, name: &CStr) -> Option<&[u8]> {
        let mut data = WebPData {
            bytes: std::ptr::null(),
            size: 0,
        };
        // Chunk data stays owned by the mux until it changes.
        (unsafe { WebPMuxGetChunk(self.0.as_ptr(), name.as_ptr(), &mut data) }
            == WebPMuxError_WEBP_MUX_OK)
            .then(|| unsafe { slice::from_raw_parts(data.bytes, data.size) })
    }
    fn set(&mut self, name: &CStr, bytes: &[u8]) -> Result<()> {
        let data = WebPData {
            bytes: bytes.as_ptr(),
            size: bytes.len(),
        };
        if unsafe { WebPMuxSetChunk(self.0.as_ptr(), name.as_ptr(), &data, 1) }
            != WebPMuxError_WEBP_MUX_OK
        {
            return Err("cannot add WebP metadata".into());
        }
        Ok(())
    }
    fn delete(&mut self, name: &CStr) {
        unsafe { WebPMuxDeleteChunk(self.0.as_ptr(), name.as_ptr()) };
    }
    fn assemble(&self) -> Result<Vec<u8>> {
        let mut data = WebPData {
            bytes: std::ptr::null(),
            size: 0,
        };
        if unsafe { WebPMuxAssemble(self.0.as_ptr(), &mut data) } != WebPMuxError_WEBP_MUX_OK {
            return Err("cannot assemble WebP".into());
        }
        let bytes = unsafe { slice::from_raw_parts(data.bytes, data.size) }.to_vec();
        unsafe { WebPFree(data.bytes.cast_mut().cast::<c_void>()) };
        Ok(bytes)
    }
}
