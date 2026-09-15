//! Encoders. Each returns the stream bytes plus the dictionary entries that
//! describe them (`Filter`, `DecodeParms`); the caller picks the smallest.

use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use lopdf::{Dictionary, Object, Stream, dictionary};

use super::raster::{Format, Raster};

#[derive(Debug, Clone)]
pub struct Encoded {
    pub codec: &'static str,
    pub bytes: Vec<u8>,
    pub filter: Option<Object>,
    pub decode_parms: Option<Dictionary>,
}

// ---------------------------------------------------------------- Flate

/// Flate with the best PNG predictor per row (the "optimum" PNG heuristic:
/// the filter whose output has the smallest sum of absolute values), or no
/// predictor at all when that is smaller.
pub fn flate(raster: &Raster) -> Option<Encoded> {
    let plain = zlib(&raster.data)?;
    let filtered = predict_rows(raster).and_then(|rows| zlib(&rows));
    let (bytes, parms) = match filtered {
        Some(f) if f.len() < plain.len() => (
            f,
            Some(dictionary! {
                "Predictor" => 15,
                "Colors" => raster.format.samples_per_pixel() as i64,
                "BitsPerComponent" => i64::from(raster.format.bits_per_component()),
                "Columns" => i64::from(raster.width),
            }),
        ),
        _ => (plain, None),
    };
    Some(Encoded {
        codec: "FlateDecode",
        bytes,
        filter: Some(Object::Name(b"FlateDecode".to_vec())),
        decode_parms: parms,
    })
}

fn zlib(data: &[u8]) -> Option<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 2), Compression::best());
    enc.write_all(data).ok()?;
    enc.finish().ok()
}

/// PNG-predicted rows: each row prefixed with its filter type byte.
fn predict_rows(raster: &Raster) -> Option<Vec<u8>> {
    let stride = raster.format.row_bytes(raster.width);
    let bpp = if raster.format == Format::Gray1 {
        1
    } else {
        raster.format.samples_per_pixel()
    };
    let mut out = Vec::with_capacity(raster.data.len() + raster.height as usize);
    let zero = vec![0u8; stride];
    let mut prev: &[u8] = &zero;
    for row in raster.data.chunks(stride) {
        let (kind, filtered) = best_filter(row, prev, bpp);
        out.push(kind);
        out.extend(filtered);
        prev = row;
    }
    Some(out)
}

fn best_filter(row: &[u8], prev: &[u8], bpp: usize) -> (u8, Vec<u8>) {
    let candidates = [
        (0u8, row.to_vec()),
        (1, filter_sub(row, bpp)),
        (2, filter_up(row, prev)),
        (3, filter_avg(row, prev, bpp)),
        (4, filter_paeth(row, prev, bpp)),
    ];
    candidates
        .into_iter()
        .min_by_key(|(_, f)| {
            f.iter()
                .map(|&b| (b as i8).unsigned_abs() as u32)
                .sum::<u32>()
        })
        .expect("five candidates")
}

fn filter_sub(row: &[u8], bpp: usize) -> Vec<u8> {
    row.iter()
        .enumerate()
        .map(|(i, &b)| b.wrapping_sub(if i >= bpp { row[i - bpp] } else { 0 }))
        .collect()
}

fn filter_up(row: &[u8], prev: &[u8]) -> Vec<u8> {
    row.iter()
        .zip(prev)
        .map(|(&b, &p)| b.wrapping_sub(p))
        .collect()
}

fn filter_avg(row: &[u8], prev: &[u8], bpp: usize) -> Vec<u8> {
    row.iter()
        .enumerate()
        .map(|(i, &b)| {
            let left = if i >= bpp { row[i - bpp] } else { 0 } as u16;
            b.wrapping_sub(((left + prev[i] as u16) / 2) as u8)
        })
        .collect()
}

fn filter_paeth(row: &[u8], prev: &[u8], bpp: usize) -> Vec<u8> {
    row.iter()
        .enumerate()
        .map(|(i, &b)| {
            let a = if i >= bpp { row[i - bpp] } else { 0 };
            let c = if i >= bpp { prev[i - bpp] } else { 0 };
            b.wrapping_sub(paeth(a, prev[i], c))
        })
        .collect()
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = (
        (p - i16::from(a)).abs(),
        (p - i16::from(b)).abs(),
        (p - i16::from(c)).abs(),
    );
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

// ----------------------------------------------------------------- JPEG

/// JPEG through mozjpeg at `quality` (1-100). Gray, RGB and CMYK; the
/// encoder's defaults (progressive, trellis quantization, 4:2:0 chroma)
/// stand until the evals say otherwise.
pub fn jpeg(raster: &Raster, quality: u8) -> Option<Encoded> {
    let color_space = match raster.format {
        Format::Gray8 => mozjpeg::ColorSpace::JCS_GRAYSCALE,
        Format::Rgb8 => mozjpeg::ColorSpace::JCS_RGB,
        // Stored as-is with an Adobe marker; PDF readers take CMYK JPEG
        // samples directly (no inversion), as the decoder side does.
        Format::Cmyk8 => mozjpeg::ColorSpace::JCS_CMYK,
        _ => return None,
    };
    let bytes = std::panic::catch_unwind(|| -> std::io::Result<Vec<u8>> {
        let mut comp = mozjpeg::Compress::new(color_space);
        comp.set_size(raster.width as usize, raster.height as usize);
        comp.set_quality(f32::from(quality));
        let mut comp = comp.start_compress(Vec::new())?;
        comp.write_scanlines(&raster.data)?;
        comp.finish()
    })
    .ok()?
    .ok()?;
    Some(Encoded {
        codec: "DCTDecode",
        bytes,
        filter: Some(Object::Name(b"DCTDecode".to_vec())),
        decode_parms: None,
    })
}

// ------------------------------------------------------------ write back

/// Replace the stream's data and the keys that describe it. Keys that stay
/// meaningful (`ColorSpace` for indexed and device images, `SMask`,
/// `Mask`, `Interpolate`, `Intent`, `Name`) are kept; ones that described
/// the old encoding (`Decode`, `DecodeParms`, `Filter`) are replaced.
pub fn apply(stream: &mut Stream, raster: &Raster, encoded: &Encoded) {
    let stencil = is_stencil(stream);
    let dict = &mut stream.dict;
    dict.set("Width", i64::from(raster.width));
    dict.set("Height", i64::from(raster.height));
    if !stencil {
        dict.set(
            "BitsPerComponent",
            i64::from(raster.format.bits_per_component()),
        );
    }
    if !stencil && !dict.has(b"ColorSpace") {
        // Only a JPX source can lack one; the raster's format is the
        // codestream's device space.
        let name: &[u8] = match raster.format {
            Format::Gray8 | Format::Gray1 => b"DeviceGray",
            Format::Rgb8 => b"DeviceRGB",
            Format::Cmyk8 => b"DeviceCMYK",
            Format::Indexed8 => b"DeviceGray",
        };
        dict.set("ColorSpace", Object::Name(name.to_vec()));
    }
    dict.remove(b"SMaskInData");
    dict.remove(b"Decode");
    dict.remove(b"DecodeParms");
    dict.remove(b"DP");
    dict.remove(b"F");
    match &encoded.filter {
        Some(f) => dict.set("Filter", f.clone()),
        None => {
            dict.remove(b"Filter");
        }
    }
    if let Some(parms) = &encoded.decode_parms {
        dict.set("DecodeParms", Object::Dictionary(parms.clone()));
    }
    stream.set_content(encoded.bytes.clone());
}

fn is_stencil(stream: &Stream) -> bool {
    stream
        .dict
        .get(b"ImageMask")
        .and_then(Object::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn flate_with_predictor_round_trips_through_lopdf() {
        // A smooth gradient, where predictors win.
        let data: Vec<u8> = (0..64u32)
            .flat_map(|y| (0..64u32).map(move |x| ((x + y) * 2) as u8))
            .collect();
        let raster = Raster::new(64, 64, Format::Gray8, data.clone()).unwrap();
        let enc = flate(&raster).unwrap();
        assert!(
            enc.decode_parms.is_some(),
            "predictor should win on a gradient"
        );
        let mut dict = dictionary! { "Filter" => "FlateDecode" };
        dict.set("DecodeParms", enc.decode_parms.clone().unwrap());
        let stream = Stream::new(dict, enc.bytes.clone());
        assert_eq!(stream.decompressed_content().unwrap(), data);
    }

    #[test]
    fn noise_round_trips_whatever_predictor_wins() {
        let data: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let raster = Raster::new(64, 64, Format::Gray8, data.clone()).unwrap();
        let enc = flate(&raster).unwrap();
        let mut dict = dictionary! { "Filter" => "FlateDecode" };
        if let Some(p) = enc.decode_parms.clone() {
            dict.set("DecodeParms", p);
        }
        let stream = Stream::new(dict, enc.bytes.clone());
        assert_eq!(stream.decompressed_content().unwrap(), data);
    }

    #[test]
    fn jpeg_encodes_gray_and_rgb() {
        let gray = Raster::new(16, 16, Format::Gray8, (0..256).map(|i| i as u8).collect()).unwrap();
        let rgb = Raster::new(16, 16, Format::Rgb8, vec![128; 768]).unwrap();
        for r in [gray, rgb] {
            let enc = jpeg(&r, 75).unwrap();
            assert!(enc.bytes.starts_with(&[0xFF, 0xD8]), "SOI marker");
        }
        assert!(jpeg(&Raster::new(1, 1, Format::Cmyk8, vec![0; 4]).unwrap(), 75).is_some());
        assert!(jpeg(&Raster::new(8, 1, Format::Gray1, vec![0]).unwrap(), 75).is_none());
    }

    #[test]
    fn higher_quality_is_larger() {
        let data: Vec<u8> = (0..64 * 64 * 3).map(|i| (i * 7 % 251) as u8).collect();
        let r = Raster::new(64, 64, Format::Rgb8, data).unwrap();
        assert!(jpeg(&r, 90).unwrap().bytes.len() > jpeg(&r, 40).unwrap().bytes.len());
    }
}
