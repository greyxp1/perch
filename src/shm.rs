use crate::Result;
use fast_image_resize::{
    MulDiv,
    PixelType::U8x4,
    images::{Image, ImageRef},
};
use image::{DynamicImage, GenericImageView, metadata::Orientation};
use memmap2::{Mmap, MmapMut, MmapOptions};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
use std::{fs::File, sync::Arc};

pub struct Pixels {
    pub file: File,
    pub mapping: MmapMut,
    pub width: i32,
    pub height: i32,
    pub orientation: Orientation,
    pub original_size: (u32, u32),
}
impl Pixels {
    pub fn new(width: i32, height: i32) -> Result<Self> {
        let size = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .filter(|_| width > 0 && height > 0)
            .ok_or("image is too large")?;
        let file = File::from(memfd_create(
            c"perch",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )?);
        file.set_len(size as u64)?;
        fcntl_add_seals(&file, SealFlags::GROW | SealFlags::SHRINK)?;
        // This private sealed file cannot shrink while mapped.
        let mapping = unsafe {
            MmapOptions::new()
                .len(size as usize)
                .populate()
                .map_mut(&file)?
        };
        Ok(Self {
            file,
            mapping,
            width,
            height,
            orientation: Orientation::NoTransforms,
            original_size: (width as u32, height as u32),
        })
    }
    pub fn apply_color(
        &mut self,
        mut color: Option<crate::color::Transform>,
        premultiplied: bool,
    ) -> Result<()> {
        crate::color::apply_rows(
            color.as_mut(),
            &mut self.mapping,
            self.width as usize,
            premultiplied,
        )
    }
    pub fn premultiply(&mut self) -> Result<()> {
        MulDiv::default().multiply_alpha_inplace(&mut Image::from_slice_u8(
            self.width as u32,
            self.height as u32,
            &mut self.mapping,
            U8x4,
        )?)?;
        Ok(())
    }
    pub fn orient(self, orientation: Orientation) -> Result<Self> {
        if orientation == Orientation::NoTransforms {
            return Ok(self);
        }
        let mut image = DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(
                self.width as u32,
                self.height as u32,
                self.mapping.to_vec(),
            )
            .ok_or("invalid image size")?,
        );
        image.apply_orientation(orientation);
        let (w, h) = image.dimensions();
        let mut pixels = Self::new(w.try_into()?, h.try_into()?)?;
        pixels.mapping.copy_from_slice(image.as_bytes());
        pixels.orientation = orientation;
        pixels.original_size = oriented(self.original_size, orientation);
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

pub fn oriented((w, h): (u32, u32), orientation: Orientation) -> (u32, u32) {
    if orientation.to_exif() >= 5 {
        (h, w)
    } else {
        (w, h)
    }
}

pub fn expand_rgb(source: &[u8], target: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    // Detection gates the only instruction set extension used by this implementation.
    let (read, write) = if is_x86_feature_detected!("ssse3") {
        unsafe { expand_rgb_ssse3(source, target) }
    } else {
        (0, 0)
    };
    #[cfg(not(target_arch = "x86_64"))]
    let (read, write) = (0, 0);
    for (source, target) in source[read..]
        .as_chunks::<3>()
        .0
        .iter()
        .zip(target[write..].as_chunks_mut::<4>().0)
    {
        target.copy_from_slice(&[source[0], source[1], source[2], 255]);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn expand_rgb_ssse3(source: &[u8], target: &mut [u8]) -> (usize, usize) {
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
    (read, write)
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
        Ok(unsafe { MmapOptions::new().populate().map(&self.file)? })
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
        fcntl_add_seals(&self.file, SealFlags::SEAL)?;
        Ok(Arc::new(Frame {
            file: self.file,
            width: self.width,
            height: self.height,
        }))
    }
}
