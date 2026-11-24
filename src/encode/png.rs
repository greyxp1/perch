use super::Raster;
use crate::Result;
use exoquant::{Color, convert_to_indexed, ditherer::FloydSteinberg, optimizer::KMeans};
use image::metadata::Orientation;
use std::borrow::Cow;

/// Chunks that change how PNG pixels display and survive metadata stripping.
const DISPLAY: [&[u8; 4]; 15] = [
    b"tRNS", b"cHRM", b"gAMA", b"iCCP", b"sBIT", b"sRGB", b"cICP", b"mDCV", b"cLLI", b"bKGD",
    b"hIST", b"pHYs", b"acTL", b"fcTL", b"fdAT",
];

pub fn encode(raster: &Raster) -> Result<Vec<u8>> {
    optimize_encoded(write(raster, png::ColorType::Rgba, &raster.rgba, None)?)
}

/// Encodes quickly without recompressing, for the clipboard.
pub fn fast(raster: &Raster) -> Result<Vec<u8>> {
    write(raster, png::ColorType::Rgba, &raster.rgba, None)
}

/// Reduces the image to a dithered palette whose size follows the quality.
pub fn quantize(raster: &Raster, quality: u8) -> Result<Vec<u8>> {
    let pixels: Vec<_> = (raster.rgba.as_chunks::<4>().0.iter())
        .map(|p| Color::new(p[0], p[1], p[2], p[3]))
        .collect();
    let colors = (usize::from(quality) * 256).div_ceil(100).max(2);
    let (palette, indices) = convert_to_indexed(
        &pixels,
        raster.width as usize,
        colors,
        &KMeans,
        &FloydSteinberg::new(),
    );
    let plte = palette.iter().flat_map(|c| [c.r, c.g, c.b]).collect();
    let mut trns: Vec<_> = palette.iter().map(|c| c.a).collect();
    while trns.last() == Some(&255) {
        trns.pop();
    }
    optimize_encoded(write(
        raster,
        png::ColorType::Indexed,
        &indices,
        Some((plte, trns)),
    )?)
}

fn write(
    raster: &Raster,
    color: png::ColorType,
    data: &[u8],
    palette: Option<(Vec<u8>, Vec<u8>)>,
) -> Result<Vec<u8>> {
    let mut info = png::Info::with_size(raster.width, raster.height);
    info.color_type = color;
    info.icc_profile = raster.icc.as_deref().map(Cow::Borrowed);
    info.exif_metadata = raster.exif.as_deref().map(Cow::Borrowed);
    if let Some((plte, trns)) = palette {
        info.palette = Some(plte.into());
        info.trns = (!trns.is_empty()).then(|| trns.into());
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::with_info(&mut out, info)?;
    // Oxipng recompresses saved images and the clipboard favors speed.
    encoder.set_compression(png::Compression::Fastest);
    if let Some(xmp) = &raster.xmp {
        encoder.add_itxt_chunk("XML:com.adobe.xmp".into(), String::from_utf8(xmp.clone())?)?;
    }
    let mut writer = encoder.write_header()?;
    writer.write_image_data(data)?;
    writer.finish()?;
    Ok(out)
}

/// Recompresses the image data that `write` stored quickly.
fn optimize_encoded(png: Vec<u8>) -> Result<Vec<u8>> {
    Ok(oxipng::optimize_from_memory(
        &png,
        &oxipng::Options::default(),
    )?)
}

/// Optimizes losslessly, keeping the exact profile and the orientation from stripped EXIF.
pub fn optimize(bytes: &[u8], keep_metadata: bool) -> Result<Vec<u8>> {
    let mut input = Cow::Borrowed(bytes);
    if !keep_metadata {
        let mut kept = bytes.get(..8).ok_or("invalid PNG")?.to_vec();
        for (name, chunk) in chunks(bytes) {
            if name[0].is_ascii_uppercase() || DISPLAY.contains(&&name) {
                kept.extend_from_slice(chunk);
            } else if &name == b"eXIf"
                && let Some(exif) = super::orientation_exif(Orientation::from_exif_chunk(
                    &chunk[8..chunk.len() - 4],
                ))
            {
                kept.extend_from_slice(&(exif.len() as u32).to_be_bytes());
                let start = kept.len();
                kept.extend_from_slice(b"eXIf");
                kept.extend_from_slice(&exif);
                let crc = crc32(&kept[start..]);
                kept.extend_from_slice(&crc.to_be_bytes());
            }
        }
        input = Cow::Owned(kept);
    }
    // Stripping is done above because oxipng would replace sRGB-like profiles.
    Ok(oxipng::optimize_from_memory(
        &input,
        &oxipng::Options::default(),
    )?)
}

pub fn animated(bytes: &[u8]) -> bool {
    chunks(bytes).any(|(name, _)| &name == b"acTL")
}

/// Yields each chunk name with its complete length, name, data and CRC bytes.
fn chunks(bytes: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut rest = bytes.get(8..).unwrap_or_default();
    std::iter::from_fn(move || {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let chunk = rest.get(..len.checked_add(12)?)?;
        rest = &rest[chunk.len()..];
        Some((chunk[4..8].try_into().ok()?, chunk))
    })
}

fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0_u32, |crc, &byte| {
        (0..8).fold(crc ^ u32::from(byte), |crc, _| {
            (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg())
        })
    })
}
