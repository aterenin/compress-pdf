//! Write a chosen encoding back into an image stream's dictionary.

use lopdf::{Object, Stream};

use super::encode::Encoded;
use super::raster::Raster;

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
