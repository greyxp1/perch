use super::{error, first_frame};
use crate::{Result, shm::Pixels};
use image::{Delay, Frame, Frames, RgbaImage, metadata::LoopCount};
use std::{
    fs::File,
    io::{BufReader, Read},
    num::NonZeroU32,
    time::Duration,
};

struct JxlInput {
    reader: BufReader<File>,
    buffer: [u8; 4096],
    valid: usize,
}
impl JxlInput {
    /// Feeds newly read bytes and keeps the unconsumed tail. Returns false at end of input.
    fn feed(&mut self, feed: impl FnOnce(&[u8]) -> jxl_oxide::Result<usize>) -> Result<bool> {
        let n = self.reader.read(&mut self.buffer[self.valid..])?;
        if n == 0 {
            return Ok(false);
        }
        self.valid += n;
        let consumed = feed(&self.buffer[..self.valid]).map_err(error)?;
        self.buffer.copy_within(consumed..self.valid, 0);
        self.valid -= consumed;
        Ok(true)
    }
}

struct JxlFrames {
    image: jxl_oxide::JxlImage,
    index: usize,
    input: JxlInput,
    ended: bool,
}
impl JxlFrames {
    fn new(file: File, limit: usize) -> Result<Self> {
        let mut input = JxlInput {
            reader: BufReader::new(file),
            buffer: [0; 4096],
            valid: 0,
        };
        let mut uninit = jxl_oxide::JxlImage::builder().build_uninit();
        let mut image = loop {
            if !input.feed(|bytes| uninit.feed_bytes(bytes))? {
                return Err("incomplete JPEG XL header".into());
            }
            match uninit.try_init().map_err(error)? {
                jxl_oxide::InitializeResult::NeedMoreData(next) => uninit = next,
                jxl_oxide::InitializeResult::Initialized(image) => break image,
            }
        };
        if u64::from(image.width()) * u64::from(image.height()) > limit as u64 / 4 {
            return Err("image exceeds the decoded memory budget".into());
        }
        image.set_cms(jxl_oxide::Moxcms);
        image.request_color_encoding(jxl_oxide::EnumColourEncoding::srgb(
            jxl_oxide::RenderingIntent::Relative,
        ));
        Ok(Self {
            image,
            index: 0,
            input,
            ended: false,
        })
    }
}
impl Iterator for JxlFrames {
    type Item = image::ImageResult<Frame>;
    fn next(&mut self) -> Option<Self::Item> {
        while self.index >= self.image.num_loaded_keyframes() {
            if self.ended || self.image.is_loading_done() {
                return None;
            }
            let result = (|| -> Result<()> {
                if !self.input.feed(|bytes| self.image.feed_bytes(bytes))? {
                    self.ended = true;
                    self.image.finalize().map_err(error)?;
                    if self.index >= self.image.num_loaded_keyframes() {
                        return Err("incomplete JPEG XL frame".into());
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                self.ended = true;
                return Some(Err(error(e)));
            }
        }
        let index = self.index;
        self.index += 1;
        Some(
            (|| -> Result<Frame> {
                let render = self.image.render_frame(index).map_err(error)?;
                let mut stream = render.stream();
                let (w, h, channels) =
                    (stream.width(), stream.height(), stream.channels() as usize);
                let len = (w as usize)
                    .checked_mul(h as usize)
                    .and_then(|n| n.checked_mul(channels))
                    .ok_or("JPEG XL frame is too large")?;
                let mut samples = Vec::new();
                samples.try_reserve_exact(len)?;
                samples.resize(len, 0u8);
                if stream.write_to_buffer(&mut samples) != len {
                    return Err("incomplete JPEG XL frame".into());
                }
                let rgba = if channels == 4 {
                    RgbaImage::from_raw(w, h, samples).ok_or("invalid JPEG XL frame size")?
                } else {
                    let mut rgba = RgbaImage::new(w, h);
                    if channels == 3 {
                        crate::shm::expand_rgb(&samples, rgba.as_mut());
                    } else {
                        for (src, dst) in samples.chunks_exact(channels).zip(rgba.pixels_mut()) {
                            dst.0 = match channels {
                                1 => [src[0], src[0], src[0], 255],
                                2 => [src[0], src[0], src[0], src[1]],
                                _ => return Err("unsupported JPEG XL channels".into()),
                            };
                        }
                    }
                    rgba
                };
                let delay = self
                    .image
                    .image_header()
                    .metadata
                    .animation
                    .as_ref()
                    .map_or(Duration::ZERO, |a| {
                        Duration::from_secs_f64(
                            f64::from(render.duration()) * f64::from(a.tps_denominator)
                                / f64::from(a.tps_numerator.max(1)),
                        )
                    });
                Ok(Frame::from_parts(
                    rgba,
                    0,
                    0,
                    Delay::from_saturating_duration(delay),
                ))
            })()
            .map_err(error),
        )
    }
}

pub(super) fn preview(file: File, limit: usize) -> Result<(Pixels, bool)> {
    let frames = JxlFrames::new(file, limit)?;
    let animated = frames.image.image_header().metadata.animation.is_some();
    Ok((first_frame(frames)?, animated))
}

pub(super) fn frames(file: File) -> Result<(Frames<'static>, LoopCount)> {
    let frames = JxlFrames::new(file, usize::MAX)?;
    let loops = (frames.image.image_header().metadata.animation.as_ref())
        .and_then(|a| NonZeroU32::new(a.num_loops))
        .map_or(LoopCount::Infinite, LoopCount::Finite);
    Ok((Frames::new(Box::new(frames)), loops))
}
