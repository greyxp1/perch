use crate::{
    Result,
    codec_ffi::{cmsCloseProfile, cmsCreate_sRGBProfile},
};
use moxcms::{ColorProfile, DataColorSpace, Layout, Transform8BitExecutor, TransformOptions};
use std::{ffi::c_void, ptr::NonNull, sync::Arc};

pub struct Transform {
    transform: Arc<Transform8BitExecutor>,
    gray: bool,
    source: Vec<u8>,
}
impl Transform {
    pub fn new(profile: Option<&[u8]>) -> Result<Option<Self>> {
        let Some(profile) = profile else {
            return Ok(None);
        };
        let profile = ColorProfile::new_from_slice(profile)?;
        Self::profile(profile).map(Some)
    }
    pub fn profile(profile: ColorProfile) -> Result<Self> {
        let layout = match profile.color_space {
            DataColorSpace::Rgb => Layout::Rgba,
            DataColorSpace::Gray => Layout::GrayAlpha,
            // CMYK must be transformed before its decoder converts the samples to RGB.
            _ => return Err("embedded profile does not describe RGB or gray pixels".into()),
        };
        Ok(Self {
            transform: profile.create_transform_8bit(
                layout,
                &ColorProfile::new_srgb(),
                Layout::Rgba,
                TransformOptions::default(),
            )?,
            gray: layout == Layout::GrayAlpha,
            source: Vec::new(),
        })
    }
    pub fn apply(&mut self, rgba: &mut [u8], premultiplied: bool) -> Result<()> {
        self.source.clear();
        self.source.extend_from_slice(rgba);
        if premultiplied {
            for p in self.source.as_chunks_mut::<4>().0.iter_mut() {
                if p[3] != 0 && p[3] != 255 {
                    for c in 0..3 {
                        p[c] = ((u32::from(p[c]) * 255 + u32::from(p[3]) / 2) / u32::from(p[3]))
                            .min(255) as u8;
                    }
                }
            }
        }
        if self.gray {
            for i in 0..rgba.len() / 4 {
                self.source[i * 2] = self.source[i * 4];
                self.source[i * 2 + 1] = self.source[i * 4 + 3];
            }
            self.source.truncate(rgba.len() / 2);
        }
        self.transform.transform(&self.source, rgba)?;
        for (p, original) in rgba
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(self.source.chunks_exact(if self.gray { 2 } else { 4 }))
        {
            p[3] = original[original.len() - 1];
            if premultiplied {
                for c in 0..3 {
                    let n = u16::from(p[c]) * u16::from(p[3]) + 128;
                    p[c] = ((n + (n >> 8)) >> 8) as u8;
                }
            }
        }
        Ok(())
    }
}

// Apply profiles one scanline at a time to bound the transform's scratch space.
pub fn apply_rows(
    transform: Option<&mut Transform>,
    pixels: &mut [u8],
    width: usize,
    premultiplied: bool,
) -> Result<()> {
    if let Some(transform) = transform {
        for row in pixels.chunks_exact_mut(width * 4) {
            transform.apply(row, premultiplied)?;
        }
    }
    Ok(())
}

pub fn convert(image: image::DynamicImage, profile: Option<&[u8]>) -> Result<image::DynamicImage> {
    convert_with(image, Transform::new(profile)?)
}
pub fn convert_with(
    image: image::DynamicImage,
    transform: Option<Transform>,
) -> Result<image::DynamicImage> {
    if let Some(mut transform) = transform {
        let mut rgba = image.into_rgba8();
        let width = rgba.width() as usize;
        apply_rows(Some(&mut transform), &mut rgba, width, false)?;
        Ok(image::DynamicImage::ImageRgba8(rgba))
    } else {
        Ok(image)
    }
}

handle! { pub(crate) Profile(c_void) = cmsCloseProfile; }
impl Profile {
    pub(crate) fn srgb() -> Result<Self> {
        Ok(Self(
            NonNull::new(unsafe { cmsCreate_sRGBProfile() }).ok_or("cannot create sRGB profile")?,
        ))
    }
}
