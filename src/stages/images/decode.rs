//! Turn an image stream into a [`Raster`].
//!
//! Handled: raw or standard-filter samples (lopdf applies Flate, LZW,
//! RunLength, ASCII filters and PNG/TIFF predictors) at 1, 2, 4, 8 and 16
//! bits in Gray, RGB, CMYK and Indexed spaces, with `Decode` arrays; and
//! DCT via zune-jpeg for Gray and RGB. Everything else returns `Skip` with
//! the reason, and the caller keeps the image untouched.

use lopdf::Stream;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace as ZColor;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use super::classify::{ColorModel, ColorSpace, ImageInfo};
use super::raster::{Format, Raster};

const MAX_DECODED_BYTES: usize = 512 * 1024 * 1024;

/// Why an image was left alone. Shown in the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip(pub String);

impl Skip {
    fn new(reason: impl Into<String>) -> Skip {
        Skip(reason.into())
    }
}

pub fn decode(stream: &Stream, info: &ImageInfo) -> Result<Raster, Skip> {
    match info.image_codec() {
        None => decode_samples(stream, info),
        Some("DCTDecode") => decode_jpeg(stream, info),
        Some(codec) => Err(Skip::new(format!("{codec} input not decoded yet"))),
    }
}

// -------------------------------------------------------------- samples

fn decode_samples(stream: &Stream, info: &ImageInfo) -> Result<Raster, Skip> {
    let data = stream
        .decompressed_content_with_limit(MAX_DECODED_BYTES)
        .map_err(|e| Skip::new(format!("stream does not decode: {e}")))?;
    let format = sample_format(info)?;
    let components = match info.color {
        ColorSpace::Device(m) => model_components(m),
        ColorSpace::Indexed { .. } => 1,
        ColorSpace::Other(ref s) => return Err(Skip::new(format!("{s} color space"))),
    };
    let row_in = (info.width as usize * components * info.bpc as usize).div_ceil(8);
    if data.len() < row_in * info.height as usize {
        return Err(Skip::new("sample data is shorter than the image"));
    }
    let raster = if format == Format::Gray1 {
        unpack_bitonal(&data, info, row_in)
    } else {
        unpack_to_8bit(&data, info, row_in, components)
    };
    apply_decode(raster, info)
}

fn sample_format(info: &ImageInfo) -> Result<Format, Skip> {
    match (&info.color, info.bpc) {
        (_, 1) if info.class() == super::classify::Class::Bitonal => Ok(Format::Gray1),
        (ColorSpace::Indexed { .. }, 1 | 2 | 4 | 8) => Ok(Format::Indexed8),
        (ColorSpace::Device(ColorModel::Gray), 1 | 2 | 4 | 8 | 16) => Ok(Format::Gray8),
        (ColorSpace::Device(ColorModel::Rgb), 1 | 2 | 4 | 8 | 16) => Ok(Format::Rgb8),
        (ColorSpace::Device(ColorModel::Cmyk), 1 | 2 | 4 | 8 | 16) => Ok(Format::Cmyk8),
        (_, bpc) => Err(Skip::new(format!("{bpc} bits per component"))),
    }
}

fn model_components(m: ColorModel) -> usize {
    match m {
        ColorModel::Gray => 1,
        ColorModel::Rgb => 3,
        ColorModel::Cmyk => 4,
    }
}

fn unpack_bitonal(data: &[u8], info: &ImageInfo, row_in: usize) -> Raster {
    let rows = info.height as usize;
    Raster {
        width: info.width,
        height: info.height,
        format: Format::Gray1,
        data: data[..row_in * rows].to_vec(),
    }
}

/// Expand samples of any supported depth to one byte each, scaling to the
/// full 0..255 range (indices are not scaled).
fn unpack_to_8bit(data: &[u8], info: &ImageInfo, row_in: usize, components: usize) -> Raster {
    let (w, h, bpc) = (info.width as usize, info.height as usize, info.bpc as u32);
    let scale_indices = matches!(info.color, ColorSpace::Indexed { .. });
    let mut out = Vec::with_capacity(w * h * components);
    let max = (1u32 << bpc) - 1;
    for row in data.chunks(row_in).take(h) {
        let mut reader = BitReader { row, pos: 0 };
        for _ in 0..w * components {
            let v = reader.read(bpc);
            out.push(if bpc == 16 {
                (v >> 8) as u8
            } else if bpc == 8 || scale_indices {
                v as u8
            } else {
                (v * 255 / max) as u8
            });
        }
    }
    let format = match components {
        _ if scale_indices => Format::Indexed8,
        1 => Format::Gray8,
        3 => Format::Rgb8,
        _ => Format::Cmyk8,
    };
    Raster {
        width: info.width,
        height: info.height,
        format,
        data: out,
    }
}

struct BitReader<'a> {
    row: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn read(&mut self, bits: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..bits {
            let byte = self.row.get(self.pos / 8).copied().unwrap_or(0);
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        v
    }
}

/// Apply a `Decode` array. Only the identity and full inversion are
/// handled; anything else is left to the reader untouched.
fn apply_decode(mut raster: Raster, info: &ImageInfo) -> Result<Raster, Skip> {
    let Some(decode) = &info.decode else {
        return Ok(raster);
    };
    let n = raster.format.samples_per_pixel();
    let identity: Vec<f32> = (0..n).flat_map(|_| [0.0, 1.0]).collect();
    let inverted: Vec<f32> = (0..n).flat_map(|_| [1.0, 0.0]).collect();
    if raster.format == Format::Indexed8 {
        let identity_idx = decode.first() == Some(&0.0);
        return if identity_idx && decode.len() == 2 {
            Ok(raster)
        } else {
            Err(Skip::new("Decode array on indexed image"))
        };
    }
    if *decode == identity {
        Ok(raster)
    } else if *decode == inverted {
        for b in &mut raster.data {
            *b = !*b;
        }
        Ok(raster)
    } else {
        Err(Skip::new("non-trivial Decode array"))
    }
}

// ------------------------------------------------------------ codestream

const STANDARD_FILTERS: [&str; 6] = [
    "FlateDecode",
    "LZWDecode",
    "RunLengthDecode",
    "ASCII85Decode",
    "ASCIIHexDecode",
    "Fl",
];

/// The bytes the image codec sees: the stored bytes with any standard
/// filters in front of the codec applied (pdflatex wraps JPEGs in Flate).
/// The codec must be the last filter.
pub fn codestream(stream: &Stream, info: &ImageInfo) -> Result<Vec<u8>, Skip> {
    let Some((codec, wrappers)) = info.filters.split_last() else {
        return Ok(stream.content.clone());
    };
    if info.image_codec() != Some(codec.as_str()) {
        return Err(Skip::new("image codec is not the last filter"));
    }
    if wrappers.is_empty() {
        return Ok(stream.content.clone());
    }
    if !wrappers
        .iter()
        .all(|f| STANDARD_FILTERS.contains(&f.as_str()))
    {
        return Err(Skip::new("unsupported filter in front of the image codec"));
    }
    let mut dict = lopdf::Dictionary::new();
    let names: Vec<lopdf::Object> = wrappers
        .iter()
        .map(|f| lopdf::Object::Name(f.as_bytes().to_vec()))
        .collect();
    dict.set("Filter", names);
    if let Ok(lopdf::Object::Array(parms)) = stream.dict.get(b"DecodeParms") {
        dict.set(
            "DecodeParms",
            parms[..parms.len().min(wrappers.len())].to_vec(),
        );
    }
    let wrapped = Stream::new(dict, stream.content.clone());
    wrapped
        .decompressed_content_with_limit(MAX_DECODED_BYTES)
        .map_err(|e| Skip::new(format!("wrapper filters do not decode: {e}")))
}

// ----------------------------------------------------------------- JPEG

fn decode_jpeg(stream: &Stream, info: &ImageInfo) -> Result<Raster, Skip> {
    let wanted = match info.color {
        ColorSpace::Device(ColorModel::Gray) => (ZColor::Luma, Format::Gray8),
        ColorSpace::Device(ColorModel::Rgb) => (ZColor::RGB, Format::Rgb8),
        _ => return Err(Skip::new("JPEG in a color space other than Gray or RGB")),
    };
    let data = codestream(stream, info)?;
    let options = DecoderOptions::new_safe()
        .jpeg_set_out_colorspace(wanted.0)
        .set_max_width(1 << 16)
        .set_max_height(1 << 16);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&data), options);
    let pixels = decoder
        .decode()
        .map_err(|e| Skip::new(format!("JPEG does not decode: {e}")))?;
    let (w, h) = decoder
        .info()
        .map(|i| (u32::from(i.width), u32::from(i.height)))
        .ok_or_else(|| Skip::new("JPEG header missing"))?;
    if (w, h) != (info.width, info.height) {
        return Err(Skip::new("JPEG size differs from the dictionary"));
    }
    let raster = Raster::new(w, h, wanted.1, pixels)
        .ok_or_else(|| Skip::new("JPEG sample count mismatch"))?;
    apply_decode(raster, info)
}

#[cfg(test)]
mod tests {
    use lopdf::dictionary;

    use super::*;

    fn info(width: u32, height: u32, bpc: u8, color: ColorSpace) -> ImageInfo {
        ImageInfo {
            width,
            height,
            bpc,
            color,
            filters: vec![],
            decode: None,
            is_stencil: false,
            has_color_key_mask: false,
        }
    }

    #[test]
    fn four_bit_gray_scales_to_full_range() {
        // Two pixels per byte: 0x0 and 0xF.
        let stream = Stream::new(dictionary! {}, vec![0x0F, 0xF0]);
        let r = decode(
            &stream,
            &info(2, 2, 4, ColorSpace::Device(ColorModel::Gray)),
        )
        .unwrap();
        assert_eq!(r.format, Format::Gray8);
        assert_eq!(r.data, vec![0, 255, 255, 0]);
    }

    #[test]
    fn sixteen_bit_rgb_keeps_the_high_byte() {
        let stream = Stream::new(dictionary! {}, vec![0x12, 0x34, 0xAB, 0xCD, 0xFF, 0x00]);
        let r = decode(
            &stream,
            &info(1, 1, 16, ColorSpace::Device(ColorModel::Rgb)),
        )
        .unwrap();
        assert_eq!(r.data, vec![0x12, 0xAB, 0xFF]);
    }

    #[test]
    fn indexed_indices_are_not_scaled() {
        let stream = Stream::new(dictionary! {}, vec![0b0001_0010]);
        let cs = ColorSpace::Indexed {
            base: ColorModel::Rgb,
            hival: 3,
        };
        let r = decode(&stream, &info(2, 1, 4, cs)).unwrap();
        assert_eq!(r.format, Format::Indexed8);
        assert_eq!(r.data, vec![1, 2]);
    }

    #[test]
    fn inverted_decode_flips_samples() {
        let stream = Stream::new(dictionary! {}, vec![0, 255]);
        let mut i = info(2, 1, 8, ColorSpace::Device(ColorModel::Gray));
        i.decode = Some(vec![1.0, 0.0]);
        assert_eq!(decode(&stream, &i).unwrap().data, vec![255, 0]);
        i.decode = Some(vec![0.2, 0.8]);
        assert!(decode(&stream, &i).is_err());
    }

    #[test]
    fn bitonal_is_kept_packed() {
        let stream = Stream::new(dictionary! {}, vec![0b1010_0000, 0b0101_0000]);
        let r = decode(
            &stream,
            &info(4, 2, 1, ColorSpace::Device(ColorModel::Gray)),
        )
        .unwrap();
        assert_eq!(r.format, Format::Gray1);
        assert_eq!(r.data, vec![0b1010_0000, 0b0101_0000]);
    }

    #[test]
    fn short_data_is_skipped() {
        let stream = Stream::new(dictionary! {}, vec![0; 5]);
        assert!(decode(&stream, &info(2, 2, 8, ColorSpace::Device(ColorModel::Rgb))).is_err());
    }
}
