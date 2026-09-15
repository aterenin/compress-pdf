//! Bitonal codecs: CCITT and JBIG2 input decoding, CCITT G4 and JBIG2
//! (generic region, lossless) output.
//!
//! Raster convention throughout is PDF DeviceGray at 1 bit: 1 = white,
//! 0 = black, rows padded to a byte. The filters' own conventions
//! (`BlackIs1`, JBIG2's 1 = black) are translated at the boundary.
//!
//! JBIG2 output uses generic-region coding because the available encoder's
//! symbol mode substitutes glyphs (lossy) and has no refinement; see the
//! provisional choices in CLAUDE.md.

use fax::encoder::Encoder as G4Encoder;
use fax::{Color, VecWriter};
use hayro_ccitt::{DecodeSettings, DecoderContext, EncodingMode};
use lopdf::{Dictionary, Object, dictionary};

use super::decode::Skip;
use super::encode::Encoded;
use super::transform::{Format, Raster};

/// Collects decoder callbacks into packed 1-bit rows.
struct BitCollector {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
    x: usize,
    y: usize,
}

impl BitCollector {
    fn new(width: u32, height: u32) -> BitCollector {
        let stride = Format::Gray1.row_bytes(width);
        BitCollector {
            width,
            height,
            stride,
            data: vec![0; stride * height as usize],
            x: 0,
            y: 0,
        }
    }

    fn push(&mut self, white: bool) {
        if self.y < self.height as usize && self.x < self.width as usize {
            if white {
                self.data[self.y * self.stride + self.x / 8] |= 0x80 >> (self.x % 8);
            }
            self.x += 1;
        }
    }

    fn push_run(&mut self, white: bool, pixels: usize) {
        for _ in 0..pixels {
            self.push(white);
        }
    }

    fn end_line(&mut self) {
        self.x = 0;
        self.y += 1;
    }

    fn finish(self) -> Result<Raster, Skip> {
        if self.y < self.height as usize {
            return Err(Skip(format!("decoded {} of {} rows", self.y, self.height)));
        }
        Raster::new(self.width, self.height, Format::Gray1, self.data)
            .ok_or_else(|| Skip("bitonal size mismatch".into()))
    }
}

struct CcittSink {
    bits: BitCollector,
    black_is_1: bool,
}

impl hayro_ccitt::Decoder for CcittSink {
    fn push_pixel(&mut self, white: bool) {
        let set = if self.black_is_1 { !white } else { white };
        self.bits.push(set);
    }
    fn push_pixel_chunk(&mut self, white: bool, chunk_count: u32) {
        let set = if self.black_is_1 { !white } else { white };
        self.bits.push_run(set, chunk_count as usize * 8);
    }
    fn next_line(&mut self) {
        self.bits.end_line();
    }
}

struct Jbig2Sink(BitCollector);

impl hayro_jbig2::Decoder for Jbig2Sink {
    fn push_pixel(&mut self, black: bool) {
        self.0.push(!black);
    }
    fn push_pixel_chunk(&mut self, black: bool, chunk_count: u32) {
        self.0.push_run(!black, chunk_count as usize * 8);
    }
    fn next_line(&mut self) {
        self.0.end_line();
    }
}

// --------------------------------------------------------------- decode

/// Decode CCITT data with the filter's `DecodeParms`. The output follows
/// the filter's `BlackIs1` convention, exactly as a reader would see it,
/// so any `Decode` array applies on top as usual.
pub fn decode_ccitt(
    data: &[u8],
    parms: Option<&Dictionary>,
    width: u32,
    height: u32,
) -> Result<Raster, Skip> {
    let int = |key: &[u8], default: i64| {
        parms
            .and_then(|p| p.get(key).and_then(Object::as_i64).ok())
            .unwrap_or(default)
    };
    let flag = |key: &[u8], default: bool| {
        parms
            .and_then(|p| p.get(key).and_then(Object::as_bool).ok())
            .unwrap_or(default)
    };
    let k = int(b"K", 0);
    let columns = int(b"Columns", 1728).max(1) as u32;
    if columns != width {
        return Err(Skip("CCITT Columns differs from Width".into()));
    }
    let black_is_1 = flag(b"BlackIs1", false);
    let settings = DecodeSettings {
        columns,
        rows: height,
        end_of_block: flag(b"EndOfBlock", true),
        end_of_line: flag(b"EndOfLine", false),
        rows_are_byte_aligned: flag(b"EncodedByteAlign", false),
        encoding: match k {
            k if k < 0 => EncodingMode::Group4,
            0 => EncodingMode::Group3_1D,
            k => EncodingMode::Group3_2D { k: k as u32 },
        },
        invert_black: false,
    };
    let mut sink = CcittSink {
        bits: BitCollector::new(width, height),
        black_is_1,
    };
    let mut ctx = DecoderContext::new(settings);
    hayro_ccitt::decode(data, &mut sink, &mut ctx).map_err(|e| Skip(format!("CCITT: {e:?}")))?;
    sink.bits.finish()
}

/// Decode embedded JBIG2 data (plus the optional globals stream). Output is
/// in DeviceGray convention (the filter inverts JBIG2's 1 = black).
pub fn decode_jbig2(
    data: &[u8],
    globals: Option<&[u8]>,
    width: u32,
    height: u32,
) -> Result<Raster, Skip> {
    let image = hayro_jbig2::Image::new_embedded(data, globals)
        .map_err(|e| Skip(format!("JBIG2: {e:?}")))?;
    // Codestreams are sometimes padded a little wider than the dictionary
    // says (encoders round up to bytes); the collector drops the excess.
    // Smaller than declared would leave rows undefined, so that is refused.
    if image.width() < width || image.height() < height {
        return Err(Skip(format!(
            "JBIG2 codestream is {}x{} but the dictionary says {width}x{height}",
            image.width(),
            image.height()
        )));
    }
    let mut sink = Jbig2Sink(BitCollector::new(width, height));
    image
        .decode(&mut sink)
        .map_err(|e| Skip(format!("JBIG2: {e:?}")))?;
    sink.0.finish()
}

// --------------------------------------------------------------- encode

/// CCITT Group 4 with `BlackIs1 false`, so the stream decodes straight
/// into DeviceGray convention.
pub fn encode_g4(raster: &Raster) -> Option<Encoded> {
    if raster.format != Format::Gray1 {
        return None;
    }
    let stride = raster.format.row_bytes(raster.width);
    let mut encoder = G4Encoder::new(VecWriter::new());
    for row in raster.data.chunks(stride) {
        let pels = (0..raster.width as usize).map(|x| {
            if (row[x / 8] >> (7 - x % 8)) & 1 == 1 {
                Color::White
            } else {
                Color::Black
            }
        });
        encoder.encode_line(pels, raster.width).ok()?;
    }
    let bytes = encoder.finish().ok()?.finish();
    Some(Encoded {
        codec: "CCITTFaxDecode",
        bytes,
        filter: Some(Object::Name(b"CCITTFaxDecode".to_vec())),
        decode_parms: Some(dictionary! {
            "K" => -1,
            "Columns" => i64::from(raster.width),
            "Rows" => i64::from(raster.height),
            "BlackIs1" => false,
        }),
    })
}

/// JBIG2 generic region (lossless), embedded organisation, no globals.
pub fn encode_jbig2(raster: &Raster) -> Option<Encoded> {
    if raster.format != Format::Gray1 {
        return None;
    }
    let unpacked: Vec<u8> = raster
        .gray1_to_gray8()
        .data
        .iter()
        .map(|&v| u8::from(v == 0))
        .collect();
    let result =
        jbig2enc_rust::encode_single_image_lossless(&unpacked, raster.width, raster.height, true)
            .ok()?;
    if result.global_data.as_ref().is_some_and(|g| !g.is_empty()) {
        return None;
    }
    Some(Encoded {
        codec: "JBIG2Decode",
        bytes: result.page_data,
        filter: Some(Object::Name(b"JBIG2Decode".to_vec())),
        decode_parms: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64x16 test pattern: black diagonal band and a vertical bar.
    fn pattern() -> Raster {
        let mut gray = vec![255u8; 64 * 16];
        for y in 0..16 {
            for x in 0..64 {
                if (x + y) % 9 < 3 || (30..34).contains(&x) {
                    gray[y * 64 + x] = 0;
                }
            }
        }
        Raster::new(64, 16, Format::Gray8, gray)
            .unwrap()
            .gray8_to_gray1()
    }

    #[test]
    fn g4_round_trips_through_hayro_ccitt() {
        let raster = pattern();
        let enc = encode_g4(&raster).unwrap();
        let parms = enc.decode_parms.clone().unwrap();
        let back = decode_ccitt(&enc.bytes, Some(&parms), 64, 16).unwrap();
        assert_eq!(back, raster);
    }

    #[test]
    fn jbig2_round_trips_through_hayro_jbig2() {
        let raster = pattern();
        let enc = encode_jbig2(&raster).unwrap();
        let back = decode_jbig2(&enc.bytes, None, 64, 16).unwrap();
        assert_eq!(back, raster);
    }

    #[test]
    fn black_is_1_is_honored_on_decode() {
        let raster = pattern();
        let enc = encode_g4(&raster).unwrap();
        let parms = dictionary! { "K" => -1, "Columns" => 64, "Rows" => 16, "BlackIs1" => true };
        let back = decode_ccitt(&enc.bytes, Some(&parms), 64, 16).unwrap();
        let inverted: Vec<u8> = raster.data.iter().map(|b| !b).collect();
        assert_eq!(back.data, inverted);
    }
}
