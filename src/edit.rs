//! Non-destructive edits, applied the same way to previews and exports.
use crate::{Result, encode::Raster};
use fast_image_resize::{
    PixelType::U8x4,
    ResizeOptions, Resizer,
    images::{Image, ImageRef},
};

/// A rectangle `(x, y, width, height)` in pixels of the rotated and flipped image.
pub type Rect = (u32, u32, u32, u32);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct EditState {
    pub crop: Option<Rect>,
    /// Clockwise quarter turns, applied before the flips.
    pub turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
    /// Output size after cropping.
    pub resize: Option<(u32, u32)>,
    pub brightness: i8,
    pub contrast: i8,
    pub saturation: i8,
}
impl EditState {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
    /// The size of `size` after rotating.
    pub fn oriented(&self, (w, h): (u32, u32)) -> (u32, u32) {
        if self.turns % 2 == 1 { (h, w) } else { (w, h) }
    }
    pub fn cropped(&self, size: (u32, u32)) -> (u32, u32) {
        self.crop.map_or(self.oriented(size), |c| (c.2, c.3))
    }
    pub fn output(&self, size: (u32, u32)) -> (u32, u32) {
        self.resize.unwrap_or(self.cropped(size))
    }
    /// The edits without tone adjustments.
    pub fn geometry(&self) -> Self {
        Self {
            brightness: 0,
            contrast: 0,
            saturation: 0,
            ..*self
        }
    }

    /// Rotates the result a quarter turn, carrying the crop and resize along.
    pub fn rotate(&mut self, size: (u32, u32), clockwise: bool) {
        let (w, h) = self.oriented(size);
        self.crop = self.crop.map(|(x, y, cw, ch)| {
            if clockwise {
                (h - y - ch, x, ch, cw)
            } else {
                (y, w - x - cw, ch, cw)
            }
        });
        self.resize = self.resize.map(|(w, h)| (h, w));
        // A flip after a quarter turn is the other flip before it.
        (self.flip_h, self.flip_v) = (self.flip_v, self.flip_h);
        self.turns = (self.turns + if clockwise { 1 } else { 3 }) % 4;
    }

    /// Mirrors the result, carrying the crop along.
    pub fn flip(&mut self, size: (u32, u32), horizontal: bool) {
        let (w, h) = self.oriented(size);
        self.crop = self.crop.map(|(x, y, cw, ch)| {
            if horizontal {
                (w - x - cw, y, cw, ch)
            } else {
                (x, h - y - ch, cw, ch)
            }
        });
        if horizontal {
            self.flip_h = !self.flip_h;
        } else {
            self.flip_v = !self.flip_v;
        }
        // Both flips are a half turn, which keeps identical results equal.
        if self.flip_h && self.flip_v {
            (self.flip_h, self.flip_v) = (false, false);
            self.turns = (self.turns + 2) % 4;
        }
    }

    /// Sets the crop, scaling any resize by the same factor as before.
    pub fn set_crop(&mut self, size: (u32, u32), crop: Option<Rect>) {
        let frame = self.oriented(size);
        let crop = crop.filter(|c| *c != (0, 0, frame.0, frame.1));
        let old = self.cropped(size);
        self.crop = crop;
        let new = self.cropped(size);
        self.resize = self.resize.map(|(w, h)| {
            let side = |n: u32, from: u32, to: u32| {
                ((f64::from(n) * f64::from(to) / f64::from(from)).round() as u32).max(1)
            };
            (side(w, old.0, new.0), side(h, old.1, new.1))
        });
    }

    /// Applies every edit at full resolution.
    pub fn apply(&self, raster: &Raster) -> Result<Raster> {
        let mut out = self.render(raster, self.resize)?;
        self.tone(&mut out);
        Ok(out)
    }

    /// Rotates, flips, crops and resizes to `size`, or the cropped size, without tone.
    pub fn render(&self, raster: &Raster, size: Option<(u32, u32)>) -> Result<Raster> {
        let source = (raster.width, raster.height);
        let frame = self.oriented(source);
        let (x, y, w, h) = self.crop.unwrap_or((0, 0, frame.0, frame.1));
        if x.checked_add(w).is_none_or(|r| r > frame.0)
            || y.checked_add(h).is_none_or(|b| b > frame.1)
            || w == 0
            || h == 0
        {
            return Err("the crop is outside the image".into());
        }
        // Resizing commutes with rotating and flipping, so the crop resizes in
        // source orientation and only the smaller result is reoriented.
        let (sx, sy, sw, sh) = self.unorient((x, y, w, h), source);
        let (tw, th) = self.oriented(size.unwrap_or((w, h)));
        let rgba = if (sw, sh) == (tw, th) {
            let mut rgba = Vec::with_capacity(sw as usize * sh as usize * 4);
            for row in sy..sy + sh {
                let start = (row as usize * source.0 as usize + sx as usize) * 4;
                rgba.extend_from_slice(&raster.rgba[start..start + sw as usize * 4]);
            }
            rgba
        } else {
            let mut target = Image::new(tw.max(1), th.max(1), U8x4);
            Resizer::new().resize(
                &ImageRef::new(source.0, source.1, &raster.rgba, U8x4)?,
                &mut target,
                &ResizeOptions::new().crop(
                    f64::from(sx),
                    f64::from(sy),
                    f64::from(sw),
                    f64::from(sh),
                ),
            )?;
            target.into_vec()
        };
        let (width, height, rgba) = self.orient(tw.max(1), th.max(1), &rgba);
        Ok(Raster {
            width,
            height,
            rgba,
            icc: raster.icc.clone(),
            exif: raster.exif.clone(),
            xmp: raster.xmp.clone(),
        })
    }

    /// Maps a rectangle of the result back into the unrotated source of `size`.
    fn unorient(&self, (mut x, mut y, mut w, mut h): Rect, size: (u32, u32)) -> Rect {
        let (fw, fh) = self.oriented(size);
        if self.flip_h {
            x = fw - x - w;
        }
        if self.flip_v {
            y = fh - y - h;
        }
        // Each step undoes one clockwise turn of an image `width` wide.
        let mut width = fw;
        for _ in 0..self.turns {
            (x, y, w, h) = (y, width - x - w, h, w);
            width = if width == fw { fh } else { fw };
        }
        (x, y, w, h)
    }

    /// Rotates and flips pixels of the unrotated size `(w, h)`.
    fn orient(&self, w: u32, h: u32, rgba: &[u8]) -> (u32, u32, Vec<u8>) {
        if self.turns == 0 && !self.flip_h && !self.flip_v {
            return (w, h, rgba.to_vec());
        }
        let (ow, oh) = self.oriented((w, h));
        // The source pixel of output pixel (x, y), as an affine map of both.
        let source = |x: i64, y: i64| {
            let (mut x, mut y) = (x, y);
            if self.flip_h {
                x = i64::from(ow) - 1 - x;
            }
            if self.flip_v {
                y = i64::from(oh) - 1 - y;
            }
            let mut width = i64::from(ow);
            for _ in 0..self.turns {
                let height = if width == i64::from(ow) {
                    i64::from(oh)
                } else {
                    i64::from(ow)
                };
                (x, y) = (y, width - 1 - x);
                width = height;
            }
            y * i64::from(w) + x
        };
        let origin = source(0, 0);
        let (dx, dy) = (source(1, 0) - origin, source(0, 1) - origin);
        let pixels = rgba.as_chunks::<4>().0;
        let mut out = Vec::with_capacity(pixels.len() * 4);
        for row in 0..i64::from(oh) {
            let mut index = origin + row * dy;
            for _ in 0..ow {
                out.extend_from_slice(&pixels[index as usize]);
                index += dx;
            }
        }
        (ow, oh, out)
    }

    /// Adjusts brightness, contrast and saturation of straight RGBA, keeping alpha.
    pub fn tone(&self, raster: &mut Raster) {
        if (self.brightness, self.contrast, self.saturation) == (0, 0, 0) {
            return;
        }
        let offset = f32::from(self.brightness) / 250.0;
        let gain = ((100.0 + f32::from(self.contrast)) / 100.0).powi(2);
        let lut: [u8; 256] = std::array::from_fn(|v| {
            let v = (v as f32 / 255.0 - 0.5) * gain + 0.5 + offset;
            (v.clamp(0.0, 1.0) * 255.0).round() as u8
        });
        let saturation = 256 + i32::from(self.saturation) * 256 / 100;
        for p in raster.rgba.as_chunks_mut::<4>().0 {
            let [r, g, b] = [lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]];
            if saturation == 256 {
                (p[0], p[1], p[2]) = (r, g, b);
                continue;
            }
            let luma = (i32::from(r) * 54 + i32::from(g) * 183 + i32::from(b) * 19 + 128) >> 8;
            for (c, v) in p.iter_mut().zip([r, g, b]) {
                *c = (luma + (((i32::from(v) - luma) * saturation) >> 8)).clamp(0, 255) as u8;
            }
        }
    }
}

/// Edits with bounded undo and redo.
#[derive(Default)]
pub struct History {
    pub current: EditState,
    undo: Vec<EditState>,
    redo: Vec<EditState>,
}
impl History {
    const DEPTH: usize = 100;
    /// Records the current state so later direct changes undo together.
    pub fn checkpoint(&mut self) {
        if self.undo.len() == Self::DEPTH {
            self.undo.remove(0);
        }
        self.undo.push(self.current);
        self.redo.clear();
    }
    /// Applies `edit` as one undoable step, returning whether it changed anything.
    pub fn change(&mut self, edit: impl FnOnce(&mut EditState)) -> bool {
        let before = self.current;
        edit(&mut self.current);
        if self.current == before {
            return false;
        }
        let after = std::mem::replace(&mut self.current, before);
        self.checkpoint();
        self.current = after;
        true
    }
    pub fn can_undo(&self) -> bool {
        self.undo.iter().any(|s| *s != self.current)
    }
    pub fn can_redo(&self) -> bool {
        self.redo.iter().any(|s| *s != self.current)
    }
    /// Steps back to the last different state, returning whether one existed.
    pub fn undo(&mut self) -> bool {
        Self::step(&mut self.current, &mut self.undo, &mut self.redo)
    }
    pub fn redo(&mut self) -> bool {
        Self::step(&mut self.current, &mut self.redo, &mut self.undo)
    }
    fn step(current: &mut EditState, from: &mut Vec<EditState>, to: &mut Vec<EditState>) -> bool {
        while let Some(state) = from.pop() {
            if state != *current {
                to.push(std::mem::replace(current, state));
                return true;
            }
        }
        false
    }
    pub fn reset(&mut self) -> bool {
        self.change(|s| *s = EditState::default())
    }
}
