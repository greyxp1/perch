use crate::{
    Result,
    codec_ffi::{
        TIFF, TIFFClose, TIFFCurrentDirOffset, TIFFFdOpen, TIFFGetField, TIFFGetStrileByteCount,
        TIFFGetStrileOffset, TIFFIsCODECConfigured, TIFFIsTiled, TIFFRGBAImageOK,
        TIFFReadDirectory, TIFFReadRGBAImageOriented, TIFFReadRGBAStripExt, TIFFReadRGBATileExt,
        TIFFReadScanline, TIFFSetField, TIFFSetSubDirectory,
    },
    shm::Pixels,
};
use image::{DynamicImage, metadata::Orientation};
use std::{
    fs::File,
    io::{BufRead, Seek},
    os::fd::{AsRawFd, IntoRawFd},
};

impl Pixels {
    /// Decodes chunky RGB8 with the Rust decoder, or returns `None` for the generic path.
    pub(super) fn tiff_rgb8(
        reader: &mut (impl BufRead + Seek),
        limit: usize,
    ) -> Result<Option<Self>> {
        let mut decoder =
            tiff::decoder::Decoder::new(&mut *reader)?.with_limits(tiff_limits(limit));
        if decoder.colortype()? == tiff::ColorType::RGB(8)
            && decoder
                .find_tag_unsigned::<u16>(tiff::tags::Tag::PlanarConfiguration)?
                .unwrap_or(1)
                == 1
        {
            let (width, height) = decoder.dimensions()?;
            let orientation = decoder
                .find_tag(tiff::tags::Tag::Orientation)?
                .and_then(|v| Orientation::from_exif(v.into_u16().ok()?.try_into().ok()?))
                .unwrap_or(Orientation::NoTransforms);
            if let tiff::decoder::DecodingResult::U8(bytes) = decoder.read_image()? {
                let image = DynamicImage::ImageRgb8(
                    image::RgbImage::from_raw(width, height, bytes).ok_or("invalid TIFF size")?,
                );
                let profile = decoder
                    .find_tag(tiff::tags::Tag::IccProfile)?
                    .map(|v| v.into_u8_vec())
                    .transpose()?;
                return Self::oriented_raster(image, profile.as_deref(), orientation).map(Some);
            }
            return Err("invalid TIFF samples".into());
        }
        Ok(None)
    }

    pub fn tiff(tiff: Tiff, size: (u32, u32)) -> Result<Self> {
        let mut pixels = Self::new(size.0.try_into()?, size.1.try_into()?)?;
        let decoded = unsafe {
            let handle = tiff.0;
            tiff.dimensions()? == size
                && TIFFSetField(handle, 274, 1_i32) == 1
                && TIFFReadRGBAImageOriented(
                    handle,
                    size.0,
                    size.1,
                    pixels.mapping.as_mut_ptr().cast(),
                    1,
                    1,
                ) == 1
        };
        if !decoded {
            return Err("cannot decode TIFF pixels".into());
        }
        pixels.apply_color(tiff.color()?, true)?;
        Ok(pixels)
    }
}

fn tiff_limits(bytes: usize) -> tiff::decoder::Limits {
    let mut limits = tiff::decoder::Limits::default();
    limits.decoding_buffer_size = bytes;
    limits.intermediate_buffer_size = bytes;
    limits.ifd_value_size = bytes;
    limits
}

struct Chunk<'a> {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    solid: Option<[u8; 4]>,
    data: &'a [u8],
}
impl Chunk<'_> {
    /// Yields the chunk rows within `top..bottom`.
    fn rows(&self, top: u32, bottom: u32) -> impl Iterator<Item = (u32, &[u8])> {
        let stride = self.width as usize * 4;
        let end = self.y.saturating_add(self.height).min(bottom);
        (self.y.max(top)..end).map(move |y| {
            let offset = (self.height - 1 - (y - self.y)) as usize * stride;
            (y, &self.data[offset..offset + stride])
        })
    }
}
pub struct Tiff(*mut TIFF);
impl Drop for Tiff {
    fn drop(&mut self) {
        unsafe { TIFFClose(self.0) }
    }
}
impl Tiff {
    pub fn open(file: File) -> Result<Self> {
        // Failed opens retain the fd, successful opens transfer it to TIFFClose.
        unsafe {
            let handle = TIFFFdOpen(file.as_raw_fd(), c"perch".as_ptr(), c"r".as_ptr());
            if handle.is_null() {
                return Err("cannot open TIFF decoder".into());
            }
            let _ = file.into_raw_fd();
            Ok(Self(handle))
        }
    }
    fn color(&self) -> Result<Option<crate::color::Transform>> {
        let (mut length, mut data) = (0_u32, std::ptr::null::<u8>());
        let mut photo = 0_u16;
        unsafe {
            TIFFGetField(self.0, 262, &mut photo);
        }
        if photo > 2 {
            return Ok(None);
        }
        if unsafe { TIFFGetField(self.0, 34675, &mut length, &mut data) } == 1 && !data.is_null() {
            // libtiff owns this profile until the current directory changes or closes.
            crate::color::Transform::new(Some(unsafe {
                std::slice::from_raw_parts(data, length as usize)
            }))
        } else {
            Ok(None)
        }
    }
    pub fn dimensions(&self) -> Result<(u32, u32)> {
        let (mut w, mut h) = (0_u32, 0_u32);
        if unsafe {
            TIFFGetField(self.0, 256, &mut w) == 1 && TIFFGetField(self.0, 257, &mut h) == 1
        } && w > 0
            && h > 0
        {
            Ok((w, h))
        } else {
            Err("invalid TIFF dimensions".into())
        }
    }
    pub fn chunk_bytes(&self, width: u32, height: u32) -> usize {
        let (mut tw, mut th) = (width, height);
        let tiled = self.tiled();
        if tiled {
            if unsafe {
                TIFFGetField(self.0, 322, &mut tw) != 1 || TIFFGetField(self.0, 323, &mut th) != 1
            } {
                return usize::MAX;
            }
        } else {
            unsafe {
                TIFFGetField(self.0, 278, &mut th);
            }
            th = th.min(height);
        }
        let bytes = (tw as usize).saturating_mul(th as usize).saturating_mul(4);
        if !tiled && bytes > crate::RENDER_BYTES / 2 {
            (width as usize).saturating_mul(4)
        } else {
            bytes
        }
    }
    pub fn orientation(&self) -> u16 {
        let mut value = 1_u16;
        unsafe {
            TIFFGetField(self.0, 274, &mut value);
        }
        value
    }
    pub fn offset(&self) -> u64 {
        unsafe { TIFFCurrentDirOffset(self.0) }
    }
    pub fn seek(&mut self, offset: u64) -> Result<()> {
        if unsafe { TIFFSetSubDirectory(self.0, offset) } == 1 {
            Ok(())
        } else {
            Err("cannot read TIFF directory".into())
        }
    }
    pub fn next(&mut self) -> bool {
        unsafe { TIFFReadDirectory(self.0) == 1 }
    }
    pub fn children(&self) -> Vec<u64> {
        let mut count = 0_u16;
        let mut offsets: *const u64 = std::ptr::null();
        if unsafe { TIFFGetField(self.0, 330, &mut count, &mut offsets) } == 1 && !offsets.is_null()
        {
            unsafe { std::slice::from_raw_parts(offsets, usize::from(count)).to_vec() }
        } else {
            Vec::new()
        }
    }
    pub fn reduced(&self) -> bool {
        let mut kind = 0_u32;
        unsafe {
            TIFFGetField(self.0, 254, &mut kind);
        }
        kind & 1 != 0
    }
    pub fn tiled(&self) -> bool {
        unsafe { TIFFIsTiled(self.0) != 0 }
    }
    pub fn readable(&self) -> bool {
        let mut message = [0 as std::ffi::c_char; 1024];
        let mut compression = 1_u16;
        unsafe {
            TIFFGetField(self.0, 259, &mut compression);
            TIFFIsCODECConfigured(compression) != 0
                && TIFFRGBAImageOK(self.0, message.as_mut_ptr()) == 1
        }
    }
    pub fn region(
        &mut self,
        rect: (u32, u32, u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Pixels> {
        let (left, top, right, bottom) = rect;
        let mut out = Pixels::new((right - left).try_into()?, (bottom - top).try_into()?)?;
        self.visit(rect, cancelled, |tile| {
            let start = tile.x.max(left);
            let end = tile.x.saturating_add(tile.width).min(right);
            for (y, row) in tile.rows(top, bottom) {
                let row = &row[(start - tile.x) as usize * 4..(end - tile.x) as usize * 4];
                let offset =
                    ((y - top) as usize * (right - left) as usize + (start - left) as usize) * 4;
                out.mapping[offset..offset + row.len()].copy_from_slice(row);
            }
            Ok(())
        })?;
        out.apply_color(self.color()?, true)?;
        Ok(out)
    }
    pub fn overview(
        &mut self,
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Pixels> {
        let mut overview = crate::overview::Overview::new(rect, size, !self.tiled())?;
        self.visit(rect, cancelled, |tile| {
            if let Some(pixel) = tile.solid {
                overview.solid(
                    (
                        tile.x,
                        tile.y,
                        tile.x.saturating_add(tile.width),
                        tile.y.saturating_add(tile.height),
                    ),
                    pixel,
                );
            } else {
                for (y, row) in tile.rows(rect.1, rect.3) {
                    // A mixed single-row chunk is a mixed row.
                    if tile.height == 1 {
                        overview.mixed(tile.x, y, row.as_chunks().0)?;
                    } else {
                        overview.row(tile.x, y, row)?;
                    }
                }
            }
            Ok(())
        })?;
        let mut out = overview.finish()?;
        out.apply_color(self.color()?, true)?;
        Ok(out)
    }
    fn scanlines(
        &mut self,
        rect: (u32, u32, u32, u32),
        rows: u32,
        cancelled: &dyn Fn() -> bool,
        visit: &mut impl FnMut(Chunk<'_>) -> Result<()>,
    ) -> Result<bool> {
        let (mut bits, mut samples, mut photo, mut planar, mut sample_format) =
            (1_u16, 1_u16, 0_u16, 1_u16, 1_u16);
        unsafe {
            TIFFGetField(self.0, 258, &mut bits);
            TIFFGetField(self.0, 277, &mut samples);
            TIFFGetField(self.0, 262, &mut photo);
            TIFFGetField(self.0, 284, &mut planar);
            TIFFGetField(self.0, 339, &mut sample_format);
        }
        let colors = if photo == 2 { 3 } else { 1 };
        let alpha = if samples == colors + 1 {
            let mut count = 0_u16;
            let mut values: *const u16 = std::ptr::null();
            if unsafe { TIFFGetField(self.0, 338, &mut count, &mut values) } != 1
                || count != 1
                || values.is_null()
            {
                return Ok(false);
            }
            let kind = unsafe { *values };
            if !matches!(kind, 1 | 2) {
                return Ok(false);
            }
            kind
        } else if samples == colors {
            0
        } else {
            return Ok(false);
        };
        if planar != 1 || sample_format != 1 || !matches!(bits, 8 | 16) || photo > 2 {
            return Ok(false);
        }
        let width = self.dimensions()?.0;
        let stride = usize::from(samples) * usize::from(bits / 8);
        let len = (width as usize)
            .checked_mul(stride)
            .ok_or("TIFF row is too large")?;
        let mut row = Vec::new();
        row.try_reserve_exact(len)?;
        row.resize(len, 0_u8);
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(width as usize)?;
        rgba.resize(width as usize, [0_u8; 4]);
        for y in rect.1 / rows * rows..rect.3 {
            if cancelled() {
                return Err("image loading cancelled".into());
            }
            if unsafe { TIFFReadScanline(self.0, row.as_mut_ptr().cast(), y, 0) } != 1 {
                return Err("cannot decode TIFF row".into());
            }
            if y < rect.1 {
                continue;
            }
            for (src, dst) in row.chunks_exact(stride).zip(&mut rgba) {
                let sample = |c: usize| {
                    if bits == 8 {
                        src[c]
                    } else {
                        let n = u16::from_ne_bytes([src[c * 2], src[c * 2 + 1]]);
                        if photo == 2 || c == colors as usize {
                            ((u32::from(n) + 128) / 257) as u8
                        } else {
                            (n >> 8) as u8
                        }
                    }
                };
                let (r, g, b) = if colors == 3 {
                    (sample(0), sample(1), sample(2))
                } else {
                    let g = if photo == 0 {
                        255 - sample(0)
                    } else {
                        sample(0)
                    };
                    (g, g, g)
                };
                let a = if alpha == 0 {
                    255
                } else {
                    sample(colors as usize)
                };
                *dst = [r, g, b, a];
                if alpha == 2 {
                    for channel in &mut dst[..3] {
                        let n = u16::from(*channel) * u16::from(a) + 128;
                        *channel = ((n + (n >> 8)) >> 8) as u8;
                    }
                }
            }
            let solid = rgba.iter().all(|p| *p == rgba[0]).then_some(rgba[0]);
            visit(Chunk {
                x: 0,
                y,
                width,
                height: 1,
                solid,
                data: rgba.as_flattened(),
            })?;
        }
        Ok(true)
    }
    fn visit(
        &mut self,
        rect: (u32, u32, u32, u32),
        cancelled: &dyn Fn() -> bool,
        mut visit: impl FnMut(Chunk<'_>) -> Result<()>,
    ) -> Result<()> {
        let (left, top, right, bottom) = rect;
        let (w, h) = self.dimensions()?;
        let tiled = self.tiled();
        let (mut tw, mut th, mut planar) = (w, h, 1_u16);
        if tiled {
            if unsafe {
                TIFFGetField(self.0, 322, &mut tw) != 1 || TIFFGetField(self.0, 323, &mut th) != 1
            } {
                return Err("invalid TIFF tile dimensions".into());
            }
        } else {
            unsafe {
                TIFFGetField(self.0, 278, &mut th);
            }
            th = th.min(h);
        }
        if tw == 0 || th == 0 {
            return Err("invalid TIFF chunk dimensions".into());
        }
        let bytes = u64::from(tw)
            .saturating_mul(u64::from(th))
            .saturating_mul(4);
        if bytes > isize::MAX as u64 {
            return Err("TIFF tile is too large".into());
        }
        if !tiled
            && bytes > crate::RENDER_BYTES as u64 / 2
            && self.scanlines(rect, th, cancelled, &mut visit)?
        {
            return Ok(());
        }
        let mut data = Vec::new();
        data.try_reserve_exact((bytes / 4) as usize)?;
        data.resize((bytes / 4) as usize, 0_u32);
        // Raw chunks are bottom-up associated RGBA. The gathered region is oriented once.
        if unsafe { TIFFSetField(self.0, 274, 1_i32) } != 1 {
            return Err("invalid TIFF orientation".into());
        }
        unsafe {
            TIFFGetField(self.0, 284, &mut planar);
        }
        let mut previous = None;
        let mut solid = None;
        for y in (top / th..=(bottom - 1) / th).map(|y| y * th) {
            for x in (left / tw..=(right - 1) / tw).map(|x| x * tw) {
                if cancelled() {
                    return Err("image loading cancelled".into());
                }
                let index = (y / th)
                    .checked_mul(w.div_ceil(tw))
                    .and_then(|i| i.checked_add(x / tw))
                    .ok_or("too many TIFF tiles")?;
                let key = unsafe {
                    (
                        TIFFGetStrileOffset(self.0, index),
                        TIFFGetStrileByteCount(self.0, index),
                        tw.min(w - x),
                        th.min(h - y),
                    )
                };
                // Sparse TIFFs may reference the same encoded tile many times.
                if planar != 1 || previous != Some(key) {
                    let decoded = unsafe {
                        if tiled {
                            TIFFReadRGBATileExt(self.0, x, y, data.as_mut_ptr(), 1)
                        } else {
                            TIFFReadRGBAStripExt(self.0, y, data.as_mut_ptr(), 1)
                        }
                    };
                    if decoded != 1 {
                        return Err("cannot decode TIFF chunk".into());
                    }
                    let len = if tiled {
                        data.len()
                    } else {
                        tw as usize * th.min(h - y) as usize
                    };
                    // libtiff packs RGBA into native words, so chunks borrow little-endian bytes.
                    #[cfg(target_endian = "big")]
                    for p in &mut data[..len] {
                        *p = p.to_le();
                    }
                    solid = (data[..len].iter())
                        .all(|p| *p == data[0])
                        .then(|| data[0].to_ne_bytes());
                    previous = Some(key);
                }
                visit(Chunk {
                    x,
                    y,
                    width: tw,
                    height: if tiled { th } else { th.min(h - y) },
                    solid,
                    data: bytemuck::cast_slice(&data),
                })?;
            }
        }
        Ok(())
    }
}
