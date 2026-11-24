use super::Raster;
use crate::Result;
use image::{
    ExtendedColorType, ImageEncoder,
    codecs::{
        bmp::BmpEncoder,
        gif::GifEncoder,
        ico::{IcoEncoder, IcoFrame},
        qoi::QoiEncoder,
    },
};
use std::io::Cursor;
use tiff::{
    ColorType,
    decoder::Decoder,
    encoder::{Compression, DeflateLevel, Predictor, TiffEncoder, colortype},
    tags::{Tag, Type},
};

/// `ExtraSamples` value for straight alpha.
pub const UNASSOCIATED_ALPHA: u16 = 2;

/// Writes Deflate-compressed strips with the profile and XMP tags, tagging alpha with `alpha`.
pub fn tiff(raster: &Raster, alpha: u16) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    let mut encoder = TiffEncoder::new(&mut out)?
        .with_compression(Compression::Deflate(DeflateLevel::Best))
        .with_predictor(Predictor::Horizontal);
    let (width, height) = (raster.width, raster.height);
    let tags = |directory: &mut tiff::encoder::DirectoryEncoder<'_, _, _>| -> Result<()> {
        let mut entries = tiff::Directory::empty();
        if let Some(icc) = &raster.icc {
            entries.extend([(
                Tag::IccProfile,
                directory.write_entry_bytes(Type::UNDEFINED, icc)?,
            )]);
        }
        if let Some(xmp) = &raster.xmp {
            entries.extend([(
                Tag::Unknown(700),
                directory.write_entry_bytes(Type::BYTE, xmp)?,
            )]);
        }
        directory.extend_from(&entries);
        Ok(())
    };
    if raster.opaque() {
        let mut image = encoder.new_image::<colortype::RGB8>(width, height)?;
        tags(image.encoder())?;
        image.write_data(&raster.rgb())?;
    } else {
        let mut image = encoder.new_image::<colortype::RGBA8>(width, height)?;
        image.encoder().write_tag(Tag::ExtraSamples, alpha)?;
        tags(image.encoder())?;
        image.write_data(&raster.rgba)?;
    }
    Ok(out.into_inner())
}

/// Rejects TIFFs whose pages or samples would be lost by re-encoding and returns the
/// `ExtraSamples` value of the alpha, which keeps associated alpha samples untouched.
pub fn tiff_layout(bytes: &[u8]) -> Result<u16> {
    let mut decoder = Decoder::new(Cursor::new(bytes))?;
    let extra = match decoder.find_tag(Tag::ExtraSamples)? {
        Some(value) => value.into_u16_vec()?,
        None => Vec::new(),
    };
    match (decoder.colortype()?, extra.as_slice()) {
        (ColorType::RGB(8), []) if !decoder.more_images() => Ok(UNASSOCIATED_ALPHA),
        (ColorType::RGBA(8), &[alpha @ (1 | 2)]) if !decoder.more_images() => Ok(alpha),
        _ => Err("only single-page 8-bit RGB TIFFs can be compressed".into()),
    }
}

pub fn bmp(raster: &Raster) -> Result<Vec<u8>> {
    let raster = raster.srgb()?;
    let mut out = Vec::new();
    BmpEncoder::new(&mut out).write_image(
        &raster.rgb(),
        raster.width,
        raster.height,
        ExtendedColorType::Rgb8,
    )?;
    Ok(out)
}

pub fn gif(raster: &Raster) -> Result<Vec<u8>> {
    let raster = raster.srgb()?;
    let mut out = Vec::new();
    GifEncoder::new(&mut out).encode(
        &raster.rgba,
        raster.width,
        raster.height,
        ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

/// Embeds one PNG frame, which keeps the profile.
pub fn ico(raster: &Raster) -> Result<Vec<u8>> {
    let png = super::png::encode(raster)?;
    let frame = IcoFrame::with_encoded(png, raster.width, raster.height, ExtendedColorType::Rgba8)?;
    let mut out = Vec::new();
    IcoEncoder::new(&mut out).encode_images(&[frame])?;
    Ok(out)
}

pub fn qoi(raster: &Raster) -> Result<Vec<u8>> {
    let raster = raster.srgb()?;
    let mut out = Vec::new();
    QoiEncoder::new(&mut out).write_image(
        &raster.rgba,
        raster.width,
        raster.height,
        ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}
