//! Turn an image stream into a [`Raster`].
//!
//! Handled: raw or standard-filter samples (lopdf applies Flate, LZW,
//! RunLength, ASCII filters and PNG/TIFF predictors) at 1, 2, 4, 8 and 16
//! bits in Gray, RGB, CMYK and Indexed spaces, with `Decode` arrays; DCT
//! via zune-jpeg; CCITT, JBIG2 and JPX via hayro's decoders; and
//! Separation, DeviceN and Lab samples mapped into their device
//! alternate. Everything else returns `Skip` with the reason, and the
//! caller keeps the image untouched.

use lopdf::{Dictionary, Document, Object, Stream};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace as ZColor;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use super::bitonal;
use super::classify::{self, ColorModel, ColorSpace, ImageInfo, Mapping, Memo};
use super::transform::{Format, Raster};

const MAX_DECODED_BYTES: usize = 512 * 1024 * 1024;

/// Why an image was left alone. Shown in the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip(pub String);

impl Skip {
    fn new(reason: impl Into<String>) -> Skip {
        Skip(reason.into())
    }
}

pub fn decode(doc: &Document, stream: &Stream, info: &ImageInfo) -> Result<Raster, Skip> {
    if let ColorSpace::Mapped { source, .. } = &info.color {
        return decode_mapped(doc, stream, info, source);
    }
    match info.image_codec() {
        None => decode_samples(stream, info),
        Some("DCTDecode") => decode_jpeg(stream, info),
        Some("CCITTFaxDecode") => {
            let data = codestream(stream, info)?;
            let parms = codec_parms(doc, stream, info);
            let raster = bitonal::decode_ccitt(&data, parms.as_ref(), info.width, info.height)?;
            apply_decode(raster, info)
        }
        Some("JBIG2Decode") => {
            let data = codestream(stream, info)?;
            let globals = jbig2_globals(doc, codec_parms(doc, stream, info).as_ref());
            let raster = bitonal::decode_jbig2(&data, globals.as_deref(), info.width, info.height)?;
            apply_decode(raster, info)
        }
        Some("JPXDecode") => decode_jpx(stream, info),
        Some(codec) => Err(Skip::new(format!("{codec} input not decoded yet"))),
    }
}

// ------------------------------------------------------------------ JPX

/// JPEG 2000 through hayro-jpeg2000. The codestream's own color space
/// wins (the dictionary may omit one); images with an alpha channel are
/// left alone because `SMaskInData` semantics are not implemented.
fn decode_jpx(stream: &Stream, info: &ImageInfo) -> Result<Raster, Skip> {
    use hayro_jpeg2000::{ColorSpace as JpxColor, DecodeSettings, Image};
    let data = codestream(stream, info)?;
    let image = Image::new(&data, &DecodeSettings::default())
        .map_err(|e| Skip::new(format!("JPX does not decode: {e:?}")))?;
    if image.has_alpha() {
        return Err(Skip::new("JPX with an alpha channel"));
    }
    if (image.width(), image.height()) != (info.width, info.height) {
        return Err(Skip::new("JPX size differs from the dictionary"));
    }
    let format = match image.color_space().num_channels() {
        1 => Format::Gray8,
        3 => Format::Rgb8,
        4 => Format::Cmyk8,
        n => return Err(Skip::new(format!("JPX with {n} channels"))),
    };
    if matches!(image.color_space(), JpxColor::Unknown { .. }) {
        return Err(Skip::new("JPX with an unknown color space"));
    }
    if let ColorSpace::Device(model) = info.color
        && classify::components(model) != format.samples_per_pixel()
    {
        return Err(Skip::new(
            "JPX channels differ from the dictionary color space",
        ));
    }
    let pixels = image
        .decode()
        .map_err(|e| Skip::new(format!("JPX does not decode: {e:?}")))?;
    Raster::new(info.width, info.height, format, pixels)
        .ok_or_else(|| Skip::new("JPX sample count mismatch"))
}

/// The `DecodeParms` entry that belongs to the image codec (the last
/// filter): the single dictionary, or the last array element. Either may
/// be an indirect reference.
fn codec_parms(doc: &Document, stream: &Stream, info: &ImageInfo) -> Option<Dictionary> {
    let parms = stream
        .dict
        .get(b"DecodeParms")
        .or_else(|_| stream.dict.get(b"DP"))
        .ok()?;
    let parms = doc.dereference(parms).map(|(_, o)| o).unwrap_or(parms);
    let entry = match parms {
        Object::Dictionary(_) if info.filters.len() == 1 => parms,
        Object::Array(items) => items.get(info.filters.len() - 1)?,
        _ => return None,
    };
    let entry = doc.dereference(entry).map(|(_, o)| o).unwrap_or(entry);
    entry.as_dict().ok().cloned()
}

fn jbig2_globals(doc: &Document, parms: Option<&Dictionary>) -> Option<Vec<u8>> {
    let id = parms?.get(b"JBIG2Globals").ok()?.as_reference().ok()?;
    let Ok(Object::Stream(s)) = doc.get_object(id) else {
        return None;
    };
    s.decompressed_content_with_limit(MAX_DECODED_BYTES).ok()
}

// --------------------------------------------------------------- mapped

/// Separation, DeviceN and Lab: read the samples at their stored depth
/// and push every pixel through the space's mapping, one row at a time so
/// only the output raster is held in full.
fn decode_mapped(
    doc: &Document,
    stream: &Stream,
    info: &ImageInfo,
    source: &Object,
) -> Result<Raster, Skip> {
    let mapping = Mapping::build(doc, source)
        .ok_or_else(|| Skip::new("tint transform or Lab dictionary does not parse"))?;
    let n = mapping.components;
    let (rows, bpc) = mapped_rows(stream, info, n)?;
    let decode = info
        .decode
        .clone()
        .unwrap_or_else(|| mapping.default_decode());
    if decode.len() < 2 * n {
        return Err(Skip::new("short Decode array"));
    }
    let mut memo = Memo::new(&mapping, decode, bpc);
    let (w, h) = (info.width as usize, info.height as usize);
    let mut out = Vec::with_capacity(w * h * classify::components(mapping.model));
    let mut row = vec![0u16; w * n];
    for y in 0..h {
        rows.read(y, w * n, bpc, &mut row);
        for px in row.chunks(n) {
            let mapped = memo
                .lookup(px)
                .ok_or_else(|| Skip::new("tint transform failed on a sample"))?;
            out.extend_from_slice(mapped);
        }
    }
    Raster::new(info.width, info.height, device_format(mapping.model), out)
        .ok_or_else(|| Skip::new("mapped sample count mismatch"))
}

/// The stored samples of a mapped image and their depth: packed at the
/// dictionary's depth, or one byte per sample out of a JPEG.
fn mapped_rows(stream: &Stream, info: &ImageInfo, n: usize) -> Result<(Rows, u8), Skip> {
    match info.image_codec() {
        None => {
            let data = stream
                .decompressed_content_with_limit(MAX_DECODED_BYTES)
                .map_err(|e| Skip::new(format!("stream does not decode: {e}")))?;
            let row_in = (info.width as usize * n * info.bpc as usize).div_ceil(8);
            if data.len() < row_in * info.height as usize {
                return Err(Skip::new("sample data is shorter than the image"));
            }
            Ok((Rows::Packed { data, row_in }, info.bpc))
        }
        Some("DCTDecode") => {
            let model = match n {
                1 => ColorModel::Gray,
                3 => ColorModel::Rgb,
                4 => ColorModel::Cmyk,
                _ => return Err(Skip::new(format!("JPEG with {n} components"))),
            };
            Ok((Rows::Bytes(jpeg_raster(stream, info, model)?.data), 8))
        }
        Some(codec) => Err(Skip::new(format!("{codec} in a mapped color space"))),
    }
}

fn device_format(model: ColorModel) -> Format {
    match model {
        ColorModel::Gray => Format::Gray8,
        ColorModel::Rgb => Format::Rgb8,
        ColorModel::Cmyk => Format::Cmyk8,
    }
}

/// Where a mapped image's rows come from: packed samples at any depth, or
/// one byte per sample from a codec.
enum Rows {
    Packed { data: Vec<u8>, row_in: usize },
    Bytes(Vec<u8>),
}

impl Rows {
    fn read(&self, y: usize, count: usize, bpc: u8, row: &mut [u16]) {
        match self {
            Rows::Packed { data, row_in } => {
                let mut reader = BitReader {
                    row: &data[y * row_in..(y + 1) * row_in],
                    pos: 0,
                };
                for v in row.iter_mut().take(count) {
                    *v = reader.read(u32::from(bpc)) as u16;
                }
            }
            Rows::Bytes(data) => {
                for (v, b) in row.iter_mut().zip(&data[y * count..(y + 1) * count]) {
                    *v = u16::from(*b);
                }
            }
        }
    }
}

// -------------------------------------------------------------- samples

fn decode_samples(stream: &Stream, info: &ImageInfo) -> Result<Raster, Skip> {
    let data = stream
        .decompressed_content_with_limit(MAX_DECODED_BYTES)
        .map_err(|e| Skip::new(format!("stream does not decode: {e}")))?;
    let format = sample_format(info)?;
    let components = match info.color {
        ColorSpace::Device(m) => classify::components(m),
        ColorSpace::Indexed { .. } => 1,
        ColorSpace::Mapped { components, .. } => components,
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

/// Apply a `Decode` array. Bitonal images accept the identity and full
/// inversion; indices are remapped over their stored depth; 8-bit device
/// samples go through a per-component lookup table.
fn apply_decode(mut raster: Raster, info: &ImageInfo) -> Result<Raster, Skip> {
    let Some(decode) = &info.decode else {
        return Ok(raster);
    };
    let n = raster.format.samples_per_pixel();
    if decode.len() < 2 * n {
        return Err(Skip::new("short Decode array"));
    }
    match raster.format {
        Format::Gray1 => decode_bitonal(raster, decode),
        Format::Indexed8 => {
            let max = ((1u32 << info.bpc) - 1) as f32;
            for b in &mut raster.data {
                let idx = decode[0] + f32::from(*b) * (decode[1] - decode[0]) / max;
                *b = idx.round().clamp(0.0, 255.0) as u8;
            }
            Ok(raster)
        }
        _ => {
            let luts: Vec<[u8; 256]> = (0..n)
                .map(|i| decode_lut(decode[2 * i], decode[2 * i + 1]))
                .collect();
            for (i, b) in raster.data.iter_mut().enumerate() {
                *b = luts[i % n][*b as usize];
            }
            Ok(raster)
        }
    }
}

fn decode_lut(dmin: f32, dmax: f32) -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (v, out) in lut.iter_mut().enumerate() {
        let x = dmin + v as f32 / 255.0 * (dmax - dmin);
        *out = (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    lut
}

fn decode_bitonal(mut raster: Raster, decode: &[f32]) -> Result<Raster, Skip> {
    match (decode[0], decode[1]) {
        (0.0, 1.0) => Ok(raster),
        (1.0, 0.0) => {
            for b in &mut raster.data {
                *b = !*b;
            }
            Ok(raster)
        }
        _ => Err(Skip::new("fractional Decode array on a bitonal image")),
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
    let ColorSpace::Device(model) = info.color else {
        return Err(Skip::new("JPEG in an unsupported color space"));
    };
    apply_decode(jpeg_raster(stream, info, model)?, info)
}

/// Decode the JPEG codestream into the raster format `model` implies.
fn jpeg_raster(stream: &Stream, info: &ImageInfo, model: ColorModel) -> Result<Raster, Skip> {
    let data = codestream(stream, info)?;
    let options = DecoderOptions::new_safe()
        .set_max_width(1 << 16)
        .set_max_height(1 << 16);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&data), options);
    decoder
        .decode_headers()
        .map_err(|e| Skip::new(format!("JPEG header: {e}")))?;
    let input = decoder
        .input_colorspace()
        .ok_or_else(|| Skip::new("JPEG header missing"))?;
    let (out, format) = jpeg_output(model, input)?;
    decoder.set_options(
        DecoderOptions::new_safe()
            .jpeg_set_out_colorspace(out)
            .set_max_width(1 << 16)
            .set_max_height(1 << 16),
    );
    let mut pixels = decoder
        .decode()
        .map_err(|e| Skip::new(format!("JPEG does not decode: {e}")))?;
    let (w, h) = decoder
        .info()
        .map(|i| (u32::from(i.width), u32::from(i.height)))
        .ok_or_else(|| Skip::new("JPEG header missing"))?;
    if (w, h) != (info.width, info.height) {
        return Err(Skip::new("JPEG size differs from the dictionary"));
    }
    if out == ZColor::YCCK {
        ycck_to_cmyk(&mut pixels);
    }
    Raster::new(w, h, format, pixels).ok_or_else(|| Skip::new("JPEG sample count mismatch"))
}

/// Output color space to request from the decoder and the raster format
/// it yields, given the dictionary's model and the codestream's own.
fn jpeg_output(model: ColorModel, input: ZColor) -> Result<(ZColor, Format), Skip> {
    match (model, input) {
        (ColorModel::Gray, _) => Ok((ZColor::Luma, Format::Gray8)),
        (ColorModel::Rgb, ZColor::CMYK | ZColor::YCCK) => {
            Err(Skip::new("four-component JPEG in an RGB color space"))
        }
        (ColorModel::Rgb, _) => Ok((ZColor::RGB, Format::Rgb8)),
        (ColorModel::Cmyk, ZColor::CMYK) => Ok((ZColor::CMYK, Format::Cmyk8)),
        (ColorModel::Cmyk, ZColor::YCCK) => Ok((ZColor::YCCK, Format::Cmyk8)),
        (ColorModel::Cmyk, _) => Err(Skip::new("JPEG channels differ from the CMYK color space")),
    }
}

/// The YCCK to CMYK conversion PDF readers apply (pdf.js, hayro); the
/// inversion of the color channels is part of the constants.
fn ycck_to_cmyk(pixels: &mut [u8]) {
    for c in pixels.as_chunks_mut::<4>().0 {
        let (y, cb, cr) = (f32::from(c[0]), f32::from(c[1]), f32::from(c[2]));
        c[0] = (434.456 - y - 1.402 * cr).clamp(0.0, 255.0) as u8;
        c[1] = (119.541 - y + 0.344 * cb + 0.714 * cr).clamp(0.0, 255.0) as u8;
        c[2] = (481.816 - y - 1.772 * cb).clamp(0.0, 255.0) as u8;
    }
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
            icc_profile: None,
        }
    }

    #[test]
    fn four_bit_gray_scales_to_full_range() {
        // Two pixels per byte: 0x0 and 0xF.
        let stream = Stream::new(dictionary! {}, vec![0x0F, 0xF0]);
        let r = decode(
            &Document::with_version("1.5"),
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
            &Document::with_version("1.5"),
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
        let r = decode(&Document::with_version("1.5"), &stream, &info(2, 1, 4, cs)).unwrap();
        assert_eq!(r.format, Format::Indexed8);
        assert_eq!(r.data, vec![1, 2]);
    }

    #[test]
    fn inverted_decode_flips_samples() {
        let stream = Stream::new(dictionary! {}, vec![0, 255]);
        let mut i = info(2, 1, 8, ColorSpace::Device(ColorModel::Gray));
        i.decode = Some(vec![1.0, 0.0]);
        assert_eq!(
            decode(&Document::with_version("1.5"), &stream, &i)
                .unwrap()
                .data,
            vec![255, 0]
        );
        i.decode = Some(vec![0.2, 0.8]);
        assert_eq!(
            decode(&Document::with_version("1.5"), &stream, &i)
                .unwrap()
                .data,
            vec![51, 204]
        );
    }

    #[test]
    fn indexed_decode_remaps_indices() {
        let stream = Stream::new(dictionary! {}, vec![0b0011_0000]);
        let mut i = info(
            2,
            1,
            4,
            ColorSpace::Indexed {
                base: ColorModel::Rgb,
                hival: 15,
            },
        );
        i.decode = Some(vec![15.0, 0.0]);
        let r = decode(&Document::with_version("1.5"), &stream, &i).unwrap();
        assert_eq!(r.data, vec![12, 15]);
    }

    #[test]
    fn separation_samples_land_in_the_alternate_space() {
        let mut doc = Document::with_version("1.5");
        let f = doc.add_object(Stream::new(
            dictionary! { "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()],
            "C0" => vec![1.into()], "C1" => vec![0.into()], "N" => 1 },
            vec![],
        ));
        let source = Object::Array(vec![
            "Separation".into(),
            "Spot".into(),
            "DeviceGray".into(),
            f.into(),
        ]);
        let cs = ColorSpace::Mapped {
            components: 1,
            model: ColorModel::Gray,
            source,
        };
        // 4-bit tints 0, 15, 8 -> gray 255, 0, 119.
        let stream = Stream::new(dictionary! {}, vec![0x0F, 0x80]);
        let r = decode(&doc, &stream, &info(3, 1, 4, cs)).unwrap();
        assert_eq!(r.format, Format::Gray8);
        assert_eq!(r.data, vec![255, 0, 119]);
    }

    #[test]
    fn bitonal_is_kept_packed() {
        let stream = Stream::new(dictionary! {}, vec![0b1010_0000, 0b0101_0000]);
        let r = decode(
            &Document::with_version("1.5"),
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
        assert!(
            decode(
                &Document::with_version("1.5"),
                &stream,
                &info(2, 2, 8, ColorSpace::Device(ColorModel::Rgb))
            )
            .is_err()
        );
    }
}
