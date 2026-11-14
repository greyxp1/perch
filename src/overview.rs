use crate::{Result, image::Pixels};

/// Area filtering accumulates premultiplied colors without retaining the source canvas.
pub struct Overview {
    rect: (u32, u32, u32, u32),
    size: (u32, u32),
    sums: Sums,
    line: Vec<[f64; 4]>,
}
enum Sums {
    Canvas(Vec<[f64; 4]>),
    Scanlines {
        sums: Vec<[f64; 4]>,
        pixels: Pixels,
        y: usize,
    },
}
impl Sums {
    fn row(&mut self, y: usize, width: usize) -> &mut [[f64; 4]] {
        match self {
            Self::Canvas(sums) => &mut sums[y * width..(y + 1) * width],
            Self::Scanlines {
                sums,
                pixels,
                y: previous,
            } => {
                if y != *previous {
                    assert!(y > *previous, "scanlines must arrive in order");
                    write_row(
                        &mut pixels.mapping[*previous * width * 4..(*previous + 1) * width * 4],
                        sums,
                    );
                    sums.fill([0.0; 4]);
                    *previous = y;
                }
                sums
            }
        }
    }
}
fn write_row(output: &mut [u8], sums: &[[f64; 4]]) {
    for (dst, src) in output.as_chunks_mut::<4>().0.iter_mut().zip(sums) {
        for c in 0..4 {
            dst[c] = (src[c] + 0.5) as u8;
        }
        for c in 0..3 {
            dst[c] = dst[c].min(dst[3]);
        }
    }
}
impl Overview {
    // Ordered mode requires complete scanlines, so finished accumulation rows can be reused.
    pub fn new(rect: (u32, u32, u32, u32), size: (u32, u32), ordered: bool) -> Result<Self> {
        if rect.0 >= rect.2 || rect.1 >= rect.3 || size.0 == 0 || size.1 == 0 {
            return Err("invalid overview dimensions".into());
        }
        let len = (size.0 as usize)
            .checked_mul(if ordered { 1 } else { size.1 as usize })
            .ok_or("overview is too large")?;
        let mut sums = Vec::new();
        sums.try_reserve_exact(len)?;
        sums.resize(len, [0.0; 4]);
        let sums = if ordered {
            Sums::Scanlines {
                sums,
                pixels: Pixels::new(size.0.try_into()?, size.1.try_into()?)?,
                y: 0,
            }
        } else {
            Sums::Canvas(sums)
        };
        Ok(Self {
            rect,
            size,
            sums,
            line: Vec::new(),
        })
    }
    pub fn solid(&mut self, rect: (u32, u32, u32, u32), pixel: [u8; 4]) {
        let (left, top, right, bottom) = self.rect;
        let (l, t, r, b) = (
            rect.0.max(left),
            rect.1.max(top),
            rect.2.min(right),
            rect.3.min(bottom),
        );
        if l >= r || t >= b {
            return;
        }
        let (sw, sh) = (u64::from(right - left), u64::from(bottom - top));
        let (w, h) = (u64::from(self.size.0), u64::from(self.size.1));
        let (x0, x1, y0, y1) = (
            u64::from(l - left) * w,
            u64::from(r - left) * w,
            u64::from(t - top) * h,
            u64::from(b - top) * h,
        );
        for y in y0 / sh..=(y1 - 1) / sh {
            let wy = (y1.min((y + 1) * sh) - y0.max(y * sh)) as f64 / sh as f64;
            let row = self.sums.row(y as usize, w as usize);
            for x in x0 / sw..=(x1 - 1) / sw {
                let wx = (x1.min((x + 1) * sw) - x0.max(x * sw)) as f64 / sw as f64;
                let dst = &mut row[x as usize];
                for c in 0..4 {
                    dst[c] += f64::from(pixel[c]) * wx * wy;
                }
            }
        }
    }
    pub fn row(&mut self, x: u32, y: u32, pixels: &[u8]) -> Result<()> {
        if y < self.rect.1 || y >= self.rect.3 || pixels.is_empty() {
            return Ok(());
        }
        let first: [u8; 4] = pixels[..4].try_into().unwrap();
        if pixels.as_chunks::<4>().0.iter().all(|p| p == &first) {
            self.solid((x, y, x + pixels.len() as u32 / 4, y + 1), first);
        } else {
            let pixels = pixels.as_chunks::<4>().0;
            let (left, top, right, bottom) = self.rect;
            let (l, r) = (
                x.max(left),
                x.saturating_add(pixels.len() as u32).min(right),
            );
            if l >= r {
                return Ok(());
            }
            let (sw, sh) = (u64::from(right - left), u64::from(bottom - top));
            let (w, h) = (u64::from(self.size.0), u64::from(self.size.1));
            let (x0, x1, y0, y1) = (
                u64::from(l - left) * w,
                u64::from(r - left) * w,
                u64::from(y - top) * h,
                u64::from(y - top + 1) * h,
            );
            let norm = 1.0 / sw as f64;
            self.line.clear();
            self.line
                .try_reserve(((x1 - 1) / sw - x0 / sw + 1) as usize)?;
            for xx in x0 / sw..=(x1 - 1) / sw {
                let (a, b) = (x0.max(xx * sw), x1.min((xx + 1) * sw));
                let (first, last) = (a / w, (b - 1) / w);
                let i = (first + u64::from(left) - u64::from(x)) as usize;
                let j = (last + u64::from(left) - u64::from(x)) as usize;
                let mut sum = [0_u64; 4];
                if i < j {
                    for pixel in &pixels[i + 1..j] {
                        for c in 0..4 {
                            sum[c] += u64::from(pixel[c]);
                        }
                    }
                }
                let mut color = [0.0; 4];
                for c in 0..4 {
                    let value = if i == j {
                        u64::from(pixels[i][c]) * (b - a)
                    } else {
                        u64::from(pixels[i][c]) * ((first + 1) * w - a)
                            + u64::from(pixels[j][c]) * (b - last * w)
                            + sum[c] * w
                    };
                    color[c] = value as f64 * norm;
                }
                self.line.push(color);
            }
            for yy in y0 / sh..=(y1 - 1) / sh {
                let weight = (y1.min((yy + 1) * sh) - y0.max(yy * sh)) as f64 / sh as f64;
                let row = self.sums.row(yy as usize, w as usize);
                for (dst, color) in row[(x0 / sw) as usize..].iter_mut().zip(&self.line) {
                    for c in 0..4 {
                        dst[c] += color[c] * weight;
                    }
                }
            }
        }
        Ok(())
    }
    pub fn finish(self) -> Result<Pixels> {
        match self.sums {
            Sums::Canvas(sums) => {
                let mut out = Pixels::new(self.size.0.try_into()?, self.size.1.try_into()?)?;
                write_row(&mut out.mapping, &sums);
                Ok(out)
            }
            Sums::Scanlines {
                sums,
                mut pixels,
                y,
            } => {
                let stride = self.size.0 as usize * 4;
                write_row(&mut pixels.mapping[y * stride..(y + 1) * stride], &sums);
                Ok(pixels)
            }
        }
    }
}

pub fn fit(size: (u32, u32), max_side: u32) -> (u32, u32) {
    let scale = (f64::from(max_side) / f64::from(size.0.max(size.1))).min(1.0);
    (
        (f64::from(size.0) * scale).ceil().max(1.0) as u32,
        (f64::from(size.1) * scale).ceil().max(1.0) as u32,
    )
}

pub struct Rasterizer {
    rect: (u32, u32, u32, u32),
    data: RasterData,
}
enum RasterData {
    Direct(Pixels),
    Reduced(Overview),
}
impl Rasterizer {
    pub fn new(rect: (u32, u32, u32, u32), size: (u32, u32), ordered: bool) -> Result<Self> {
        if rect.0 >= rect.2 || rect.1 >= rect.3 || size.0 == 0 || size.1 == 0 {
            return Err("invalid raster dimensions".into());
        }
        let data = if size == (rect.2 - rect.0, rect.3 - rect.1) {
            RasterData::Direct(Pixels::new(size.0.try_into()?, size.1.try_into()?)?)
        } else {
            RasterData::Reduced(Overview::new(rect, size, ordered)?)
        };
        Ok(Self { rect, data })
    }
    pub fn row(&mut self, x: u32, y: u32, pixels: &[u8]) -> Result<()> {
        if let RasterData::Reduced(overview) = &mut self.data {
            return overview.row(x, y, pixels);
        }
        if let RasterData::Direct(out) = &mut self.data {
            let (l, t, r, b) = self.rect;
            let (start, end) = (x.max(l), x.saturating_add((pixels.len() / 4) as u32).min(r));
            if y < t || y >= b || start >= end {
                return Ok(());
            }
            let src = &pixels[(start - x) as usize * 4..(end - x) as usize * 4];
            let offset = ((y - t) as usize * (r - l) as usize + (start - l) as usize) * 4;
            out.mapping[offset..offset + src.len()].copy_from_slice(src);
        }
        Ok(())
    }
    pub fn pixel(&mut self, x: u32, y: u32, pixel: [u8; 4]) -> Result<()> {
        if let RasterData::Reduced(overview) = &mut self.data {
            overview.solid((x, y, x + 1, y + 1), pixel);
            Ok(())
        } else {
            self.row(x, y, &pixel)
        }
    }
    pub fn finish(self) -> Result<Pixels> {
        match self.data {
            RasterData::Direct(out) => Ok(out),
            RasterData::Reduced(overview) => overview.finish(),
        }
    }
}
