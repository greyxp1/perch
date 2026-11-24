use crate::{Result, codecs::*, image::Pixels};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    io::Read,
    ptr::{NonNull, null_mut},
    sync::Once,
};

handle! {
    Raw(libraw_data_t) = libraw_close;
    RawImage(libraw_processed_image_t) = libraw_dcraw_clear_mem;
    Wand(MagickWand) = DestroyMagickWand;
    pub(crate) Profile(c_void) = cmsCloseProfile;
}
impl Profile {
    pub(crate) fn srgb() -> Result<Self> {
        Ok(Self(
            NonNull::new(unsafe { cmsCreate_sRGBProfile() }).ok_or("cannot create sRGB profile")?,
        ))
    }
}

unsafe extern "C" fn raw_progress(
    context: *mut c_void,
    _: LibRaw_progress,
    _: c_int,
    _: c_int,
) -> c_int {
    // The synchronous decoder retains the callback only until it is cleared.
    i32::from(unsafe { (&*context.cast::<&dyn Fn() -> bool>())() })
}

unsafe extern "C" fn photoshop_progress(
    _: *const c_char,
    _: MagickOffsetType,
    _: MagickSizeType,
    context: *mut c_void,
) -> MagickBooleanType {
    // The wand is dropped before the borrowed callback can leave scope.
    MagickBooleanType::from(!unsafe { (&*context.cast::<&dyn Fn() -> bool>())() })
}

fn srgb_profile() -> Result<Vec<u8>> {
    let profile = Profile::srgb()?;
    let mut size = 0;
    if unsafe { cmsSaveProfileToMem(profile.0.as_ptr(), null_mut(), &mut size) } == 0 {
        return Err("cannot size sRGB profile".into());
    }
    let mut bytes = vec![0; size as usize];
    if unsafe { cmsSaveProfileToMem(profile.0.as_ptr(), bytes.as_mut_ptr().cast(), &mut size) } == 0
    {
        return Err("cannot save sRGB profile".into());
    }
    bytes.truncate(size as usize);
    Ok(bytes)
}

fn photoshop(path: &CStr, pixels: &mut Pixels, cancel: &dyn Fn() -> bool) -> Result<()> {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe { MagickWandGenesis() });
    let wand = Wand(
        NonNull::new(unsafe { NewMagickWand() }).ok_or("could not allocate Photoshop decoder")?,
    );
    let handle = wand.0.as_ptr();
    let (width, height) = (pixels.width as usize, pixels.height as usize);
    let context = (&cancel as *const &dyn Fn() -> bool).cast_mut().cast();
    let fail = "could not render Photoshop composite";
    unsafe {
        MagickSetProgressMonitor(handle, Some(photoshop_progress), context);
        if MagickReadImage(handle, path.as_ptr()) == 0 {
            return Err(fail.into());
        }
        MagickSetFirstIterator(handle);
        if MagickGetImageWidth(handle) != width || MagickGetImageHeight(handle) != height {
            return Err(fail.into());
        }
        let mut length = 0;
        let embedded = MagickGetImageProfile(handle, c"icc".as_ptr(), &mut length);
        let converted = if embedded.is_null() {
            MagickTransformImageColorspace(handle, ColorspaceType_sRGBColorspace)
        } else {
            MagickRelinquishMemory(embedded.cast());
            let profile = srgb_profile()?;
            MagickProfileImage(
                handle,
                c"icc".as_ptr(),
                profile.as_ptr().cast(),
                profile.len(),
            )
        };
        if converted == 0
            || cancel()
            || MagickExportImagePixels(
                handle,
                0,
                0,
                width,
                height,
                c"RGBA".as_ptr(),
                StorageType_CharPixel,
                pixels.mapping.as_mut_ptr().cast(),
            ) == 0
        {
            return Err(fail.into());
        }
    }
    pixels.premultiply()
}

fn camera_raw(path: &CStr, limit: usize, cancel: &dyn Fn() -> bool) -> Result<Option<Pixels>> {
    let Some(ptr) = NonNull::new(unsafe { libraw_init(0) }) else {
        return Ok(None);
    };
    let raw = Raw(ptr);
    let handle = raw.0.as_ptr();
    let fail = "could not develop camera RAW image";
    let mut status = 0;
    let image = unsafe {
        if libraw_open_file(handle, path.as_ptr()) != 0 || (*handle).idata.raw_count == 0 {
            return Ok(None);
        }
        let sizes = &(*handle).sizes;
        if usize::from(sizes.width) * usize::from(sizes.height) > limit / 4 || cancel() {
            return Err(fail.into());
        }
        (*handle).params.use_camera_wb = 1;
        (*handle).params.output_color = 1;
        (*handle).params.output_bps = 8;
        (*handle).rawparams.max_raw_memory_mb =
            (limit / (1024 * 1024)).try_into().unwrap_or(u32::MAX);
        let context = (&cancel as *const &dyn Fn() -> bool).cast_mut().cast();
        libraw_set_progress_handler(handle, Some(raw_progress), context);
        let image = if libraw_unpack(handle) == 0 && libraw_dcraw_process(handle) == 0 {
            NonNull::new(libraw_dcraw_make_mem_image(handle, &mut status)).map(RawImage)
        } else {
            None
        };
        libraw_set_progress_handler(handle, None, null_mut());
        image.ok_or(fail)?
    };
    drop(raw);
    let image_ptr = image.0.as_ptr();
    // LibRaw owns this allocation independently of the decoder context.
    let (width, height, len) = unsafe {
        let w = usize::from((*image_ptr).width);
        let h = usize::from((*image_ptr).height);
        let len = (*image_ptr).data_size as usize;
        if status != 0
            || (*image_ptr).type_ != LibRaw_image_formats_LIBRAW_IMAGE_BITMAP
            || (*image_ptr).colors != 3
            || (*image_ptr).bits != 8
            || w == 0
            || h == 0
            || w * h > limit / 4
            || w * h * 3 != len
        {
            return Err(fail.into());
        }
        (w, h, len)
    };
    let mut pixels = Pixels::new(width.try_into()?, height.try_into()?)?;
    let rgb = unsafe { std::slice::from_raw_parts((&raw const (*image_ptr).data).cast(), len) };
    crate::image::expand_rgb(rgb, &mut pixels.mapping);
    Ok(Some(pixels))
}

pub fn load(mut file: File, limit: usize, cancel: &dyn Fn() -> bool) -> Result<Option<Pixels>> {
    let mut header = [0; 26];
    let length = file.read(&mut header)?;
    let path = crate::fd_path(&file);
    if length == header.len() && &header[..4] == b"8BPS" {
        let version = u16::from_be_bytes(header[4..6].try_into()?);
        if !matches!(version, 1 | 2) {
            return Err("unsupported Photoshop version".into());
        }
        let h = u32::from_be_bytes(header[14..18].try_into()?);
        let w = u32::from_be_bytes(header[18..22].try_into()?);
        if u64::from(w) * u64::from(h) > limit as u64 / 4 {
            return Err("image exceeds the decoded memory budget".into());
        }
        let mut pixels = Pixels::new(w.try_into()?, h.try_into()?)?;
        photoshop(
            &CString::new(format!("PSD:{path}[0]"))?,
            &mut pixels,
            cancel,
        )?;
        Ok(Some(pixels))
    } else {
        camera_raw(&CString::new(path)?, limit, cancel)
    }
}

pub fn extension(extension: &str) -> bool {
    matches!(
        extension,
        "heic"
            | "heif"
            | "heics"
            | "heifs"
            | "hif"
            | "avifs"
            | "psd"
            | "psb"
            | "3fr"
            | "ari"
            | "arw"
            | "bay"
            | "cap"
            | "cr2"
            | "cr3"
            | "crw"
            | "dcr"
            | "dcs"
            | "dng"
            | "erf"
            | "fff"
            | "gpr"
            | "iiq"
            | "k25"
            | "kdc"
            | "mdc"
            | "mef"
            | "mos"
            | "mrw"
            | "nef"
            | "nrw"
            | "obm"
            | "orf"
            | "pef"
            | "ptx"
            | "pxn"
            | "raf"
            | "raw"
            | "rdc"
            | "rw2"
            | "rwl"
            | "rwz"
            | "sr2"
            | "srf"
            | "srw"
            | "sti"
            | "x3f"
    )
}
