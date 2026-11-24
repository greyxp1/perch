use crate::{Result, overview::Rasterizer, shm::Pixels};
use fast_image_resize::{MulDiv, PixelType::U8x4, images::Image};
use image::metadata::Orientation;
use std::io::{BufRead, Seek};

impl Pixels {
    pub(super) fn png(reader: impl BufRead + Seek, cancelled: &dyn Fn() -> bool) -> Result<Self> {
        let mut png = Png::open(reader)?;
        let (width, height, _) = png.header();
        if u64::from(width) * u64::from(height) > crate::RENDER_BYTES as u64 / 8 {
            return Self::png_stream(
                png,
                (0, 0, width, height),
                crate::overview::fit((width, height), 2048),
                cancelled,
            );
        }
        let mut pixels = Self::new(width.try_into()?, height.try_into()?)?;
        png.decode(&mut pixels.mapping)?;
        let orientation = png.orientation()?;
        pixels.apply_color(png.color()?, false)?;
        pixels.premultiply()?;
        pixels.orient(orientation)
    }
    pub fn png_region(
        reader: impl BufRead + Seek,
        expected: (u32, u32),
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let png = Png::open(reader)?;
        let (width, height, _) = png.header();
        if (width, height) != expected {
            return Err("image changed while open".into());
        }
        Self::png_stream(png, rect, size, cancelled)
    }
    fn png_stream<R: BufRead + Seek>(
        mut png: Png<R>,
        rect: (u32, u32, u32, u32),
        size: (u32, u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let (width, height, interlaced) = png.header();
        let mut color = png.color()?;
        let mut raster = Rasterizer::new(rect, size, !interlaced)?;
        let len = (width as usize)
            .checked_mul(4)
            .ok_or("PNG row is too large")?;
        let mut row = Vec::new();
        row.try_reserve_exact(len)?;
        row.resize(len, 0);
        let alpha = MulDiv::default();
        loop {
            if cancelled() {
                return Err("image loading cancelled".into());
            }
            // Adam7 supplies contiguous samples at a fixed horizontal stride on each pass.
            let Some((y, start, step)) = png.row(&mut row)? else {
                break;
            };
            if (rect.1..rect.3).contains(&y) {
                let first = rect.0.saturating_sub(start).div_ceil(step);
                let end = rect.2.saturating_sub(start).div_ceil(step);
                if end > first {
                    let data = &mut row[first as usize * 4..end as usize * 4];
                    if let Some(color) = &mut color {
                        color.apply(data, false)?;
                    }
                    alpha.multiply_alpha_inplace(&mut Image::from_slice_u8(
                        end - first,
                        1,
                        data,
                        U8x4,
                    )?)?;
                    let x = start + first * step;
                    if step == 1 {
                        raster.row(x, y, data)?;
                    } else {
                        for (sample, &pixel) in data.as_chunks::<4>().0.iter().enumerate() {
                            raster.pixel(x + sample as u32 * step, y, pixel)?;
                        }
                    }
                }
            }
        }
        let orientation = png.orientation()?;
        let mut pixels = raster.finish()?;
        pixels.original_size = (width, height);
        pixels.orient(orientation)
    }
}

// Adam7 passes as first column, column step, first row and row step.
static ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 8, 0, 8),
    (4, 8, 0, 8),
    (0, 4, 4, 8),
    (2, 4, 0, 4),
    (0, 2, 2, 4),
    (1, 2, 0, 2),
    (0, 1, 1, 2),
];

/// PNG rows decoded to straight RGBA8.
struct Png<R: BufRead + Seek> {
    reader: png::Reader<R>,
    // The png crate does not expose the Adam7 position of each row.
    rows: Box<dyn Iterator<Item = (u32, u32, u32)>>,
}
impl<R: BufRead + Seek> Png<R> {
    fn open(reader: R) -> Result<Self> {
        let mut options = png::DecodeOptions::default();
        options.set_ignore_adler32(false);
        options.set_ignore_text_chunk(true);
        let mut decoder = png::Decoder::new_with_options(reader, options);
        // Perch budgets pixels itself, and the default limit rejects large metadata chunks.
        decoder.set_limits(png::Limits { bytes: usize::MAX });
        decoder.set_transformations(
            png::Transformations::EXPAND
                | png::Transformations::STRIP_16
                | png::Transformations::ALPHA,
        );
        let reader = decoder.read_info()?;
        let info = reader.info();
        let (width, height) = (info.width, info.height);
        let passes: &'static [_] = if info.interlaced {
            &ADAM7
        } else {
            &[(0, 1, 0, 1)]
        };
        let rows =
            passes
                .iter()
                .filter(move |pass| width > pass.0)
                .flat_map(move |&(x, dx, y, dy)| {
                    (y..height).step_by(dy as usize).map(move |y| (y, x, dx))
                });
        Ok(Self {
            reader,
            rows: Box::new(rows),
        })
    }
    fn header(&self) -> (u32, u32, bool) {
        let info = self.reader.info();
        (info.width, info.height, info.interlaced)
    }
    fn color(&self) -> Result<Option<crate::color::Transform>> {
        crate::color::Transform::new(self.reader.info().icc_profile.as_deref())
    }
    fn decode(&mut self, pixels: &mut [u8]) -> Result<()> {
        if self.reader.next_frame(pixels)?.color_type == png::ColorType::GrayscaleAlpha {
            expand_gray(pixels, pixels.len() / 4);
        }
        Ok(())
    }
    /// Decodes the next row and returns its image row, first column and column step.
    fn row(&mut self, row: &mut [u8]) -> Result<Option<(u32, u32, u32)>> {
        if self.reader.read_row(row)?.is_none() {
            return Ok(None);
        }
        let (y, x, dx) = self.rows.next().ok_or("invalid PNG rows")?;
        if self.reader.output_color_type().0 == png::ColorType::GrayscaleAlpha {
            expand_gray(row, (self.reader.info().width - x).div_ceil(dx) as usize);
        }
        Ok(Some((y, x, dx)))
    }
    fn orientation(&mut self) -> Result<Orientation> {
        // Later frames cannot change the APNG poster, so their damage must not reject it.
        if self.reader.info().animation_control.is_none() {
            self.reader.finish()?;
        } else {
            let _ = self.reader.finish();
        }
        Ok(self
            .reader
            .info()
            .exif_metadata
            .as_deref()
            .and_then(Orientation::from_exif_chunk)
            .unwrap_or(Orientation::NoTransforms))
    }
}

// Expanding from the end keeps unread gray-alpha samples intact.
fn expand_gray(data: &mut [u8], samples: usize) {
    for i in (0..samples).rev() {
        let (g, a) = (data[i * 2], data[i * 2 + 1]);
        data[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, a]);
    }
}
