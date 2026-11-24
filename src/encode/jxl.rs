use super::{CompressOptions, Raster};
use crate::{Result, codec_ffi::*};
use image::metadata::Orientation;
use jxl_oxide::{
    AuxBoxData, EnumColourEncoding, ExtraChannelType, InitializeResult, JxlImage, RenderingIntent,
    frame::Encoding, image::BitDepth,
};
use std::{
    mem::MaybeUninit,
    ptr::{NonNull, null},
};

handle! {
    Encoder(JxlEncoder) = JxlEncoderDestroy;
    Runner(std::ffi::c_void) = JxlThreadParallelRunnerDestroy;
}

/// Decodes the first frame, keeping the color space of lossless RGB and rendering others to sRGB.
pub fn load(bytes: &[u8]) -> Result<Raster> {
    raster(open(bytes)?)
}

/// Re-encodes lossless stills losslessly and other stills only for lossy requests.
pub fn compress(bytes: &[u8], options: &CompressOptions) -> Result<Vec<u8>> {
    let image = open(bytes)?;
    let metadata = &image.image_header().metadata;
    if metadata.animation.is_some() {
        return Err("animated JXL cannot be compressed without losing frames".into());
    }
    let eight = |depth| matches!(depth, BitDepth::IntegerSample { bits_per_sample } if bits_per_sample <= 8);
    if !eight(metadata.bit_depth) || !metadata.ec_info.iter().all(|ec| eight(ec.bit_depth)) {
        return Err("JXL above 8 bits per channel cannot be compressed".into());
    }
    // Rendering keeps only the color channels and one straight alpha channel.
    let straight = ExtraChannelType::Alpha {
        alpha_associated: false,
    };
    if metadata.ec_info.len() > 1 || metadata.ec_info.iter().any(|ec| ec.ty != straight) {
        return Err("JXL with extra channels other than alpha cannot be compressed".into());
    }
    let lossless = !metadata.xyb_encoded
        && image
            .frame_header(0)
            .is_some_and(|frame| frame.encoding == Encoding::Modular);
    if !options.lossy && !lossless {
        return Err("lossy JXL can only be compressed with --lossy".into());
    }
    let mut raster = raster(image)?;
    if !options.keep_metadata {
        (raster.exif, raster.xmp) = (None, None);
    }
    encode(&raster, options.quality, !options.lossy)
}

pub fn encode(raster: &Raster, quality: u8, lossless: bool) -> Result<Vec<u8>> {
    let encoder = Encoder(
        NonNull::new(unsafe { JxlEncoderCreate(null()) }).ok_or("could not create JXL encoder")?,
    );
    let enc = encoder.0.as_ptr();
    let check = |status| {
        if status == JxlEncoderStatus_JXL_ENC_SUCCESS {
            Ok(())
        } else {
            Err(format!("cannot encode JXL (error {})", unsafe {
                JxlEncoderGetError(enc)
            }))
        }
    };
    let runner = Runner(
        NonNull::new(unsafe {
            JxlThreadParallelRunnerCreate(null(), JxlThreadParallelRunnerDefaultNumWorkerThreads())
        })
        .ok_or("could not create JXL threads")?,
    );
    check(unsafe {
        JxlEncoderSetParallelRunner(enc, Some(JxlThreadParallelRunner), runner.0.as_ptr())
    })?;

    let alpha = !raster.opaque();
    // Libjxl maps quality 100 to distance 0, which is only valid for lossless frames.
    let lossless = lossless || quality == 100;
    // The initializer leaves the padding unset.
    let mut info = MaybeUninit::zeroed();
    let mut info = unsafe {
        JxlEncoderInitBasicInfo(info.as_mut_ptr());
        info.assume_init()
    };
    info.xsize = raster.width;
    info.ysize = raster.height;
    info.bits_per_sample = 8;
    info.num_color_channels = 3;
    if alpha {
        info.num_extra_channels = 1;
        info.alpha_bits = 8;
    }
    // Lossless frames keep the samples instead of converting them to XYB.
    info.uses_original_profile = i32::from(lossless);
    check(unsafe { JxlEncoderSetBasicInfo(enc, &info) })?;
    match &raster.icc {
        Some(icc) => check(unsafe { JxlEncoderSetICCProfile(enc, icc.as_ptr(), icc.len()) })?,
        None => {
            let mut color = MaybeUninit::uninit();
            check(unsafe {
                JxlColorEncodingSetToSRGB(color.as_mut_ptr(), 0);
                JxlEncoderSetColorEncoding(enc, color.as_ptr())
            })?;
        }
    }

    if raster.exif.is_some() || raster.xmp.is_some() {
        check(unsafe { JxlEncoderUseBoxes(enc) })?;
        if let Some(exif) = &raster.exif {
            // The box starts with the offset of the TIFF header.
            let mut data = vec![0; 4];
            data.extend_from_slice(exif);
            check(unsafe {
                JxlEncoderAddBox(enc, c"Exif".as_ptr(), data.as_ptr(), data.len(), 0)
            })?;
        }
        if let Some(xmp) = &raster.xmp {
            check(unsafe { JxlEncoderAddBox(enc, c"xml ".as_ptr(), xmp.as_ptr(), xmp.len(), 0) })?;
        }
        unsafe { JxlEncoderCloseBoxes(enc) };
    }

    let settings = unsafe { JxlEncoderFrameSettingsCreate(enc, null()) };
    if settings.is_null() {
        return Err("could not create JXL frame settings".into());
    }
    check(unsafe {
        if lossless {
            JxlEncoderSetFrameLossless(settings, 1)
        } else {
            JxlEncoderSetFrameDistance(settings, JxlEncoderDistanceFromQuality(quality.into()))
        }
    })?;
    let pixels = if alpha {
        raster.rgba.clone()
    } else {
        raster.rgb()
    };
    let format = JxlPixelFormat {
        num_channels: if alpha { 4 } else { 3 },
        data_type: JxlDataType_JXL_TYPE_UINT8,
        endianness: JxlEndianness_JXL_NATIVE_ENDIAN,
        align: 0,
    };
    check(unsafe {
        JxlEncoderAddImageFrame(settings, &format, pixels.as_ptr().cast(), pixels.len())
    })?;
    drop(pixels);
    unsafe { JxlEncoderCloseInput(enc) };

    let mut out = vec![0; 64 * 1024];
    let mut used = 0;
    loop {
        let mut next = unsafe { out.as_mut_ptr().add(used) };
        let mut available = out.len() - used;
        let status = unsafe { JxlEncoderProcessOutput(enc, &mut next, &mut available) };
        used = out.len() - available;
        if status != JxlEncoderStatus_JXL_ENC_NEED_MORE_OUTPUT {
            check(status)?;
            break;
        }
        out.resize(out.len() * 2, 0);
    }
    out.truncate(used);
    Ok(out)
}

fn open(bytes: &[u8]) -> Result<JxlImage> {
    // The reader stops after the codestream, so feeding the whole file also finds later boxes.
    let mut uninit = JxlImage::builder().build_uninit();
    uninit.feed_bytes(bytes).map_err(|e| e.to_string())?;
    let InitializeResult::Initialized(mut image) = uninit.try_init().map_err(|e| e.to_string())?
    else {
        return Err("truncated JXL".into());
    };
    image.finalize().map_err(|e| e.to_string())?;
    image.set_cms(jxl_oxide::Moxcms);
    Ok(image)
}

fn raster(mut image: JxlImage) -> Result<Raster> {
    let metadata = &image.image_header().metadata;
    let (xyb, srgb) = (metadata.xyb_encoded, metadata.colour_encoding.is_srgb());
    let format = image.pixel_format();
    let icc = if srgb {
        None
    } else if xyb || format.is_grayscale() || format.has_black() {
        // Jxl-oxide only matches libjxl when it renders XYB to sRGB.
        image.request_color_encoding(EnumColourEncoding::srgb(RenderingIntent::Relative));
        None
    } else {
        Some(match image.original_icc() {
            Some(icc) => icc.to_vec(),
            None => image.rendered_icc(),
        })
    };
    if image.num_loaded_keyframes() == 0 {
        return Err("JXL has no displayable frames".into());
    }
    let rgba = crate::decode::jxl::rgba(&image.render_frame(0).map_err(|e| e.to_string())?)?;
    let boxes = image.aux_boxes();
    let exif = match boxes.first_exif().map_err(|e| e.to_string())? {
        AuxBoxData::Data(exif) => {
            let mut tiff = exif.payload()[exif.tiff_header_offset() as usize..].to_vec();
            // The codestream orientation, which supersedes the EXIF one, is already applied.
            let _ = Orientation::remove_from_exif_chunk(&mut tiff);
            Some(tiff)
        }
        _ => None,
    };
    let xmp = match boxes.first_xml() {
        AuxBoxData::Data(xmp) => Some(xmp.to_vec()),
        _ => None,
    };
    let mut raster = Raster {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
        icc,
        exif,
        xmp,
    };
    raster.fit_profile()?;
    Ok(raster)
}
