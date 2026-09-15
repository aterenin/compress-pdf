//! Read an image XObject's dictionary into a plain description and decide
//! which class (bitonal, indexed, continuous) it belongs to.

use lopdf::{Dictionary, Document, Object};

/// Device color model the samples are in, after resolving ICCBased by its
/// component count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorModel {
    Gray,
    Rgb,
    Cmyk,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColorSpace {
    Device(ColorModel),
    /// Palette lookup into `base`; `hival` is the highest valid index.
    Indexed {
        base: ColorModel,
        hival: u8,
    },
    /// Anything the stage does not handle yet (Separation, DeviceN, Lab...).
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Bitonal,
    Indexed,
    Gray,
    Color,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub bpc: u8,
    pub color: ColorSpace,
    pub filters: Vec<String>,
    pub decode: Option<Vec<f32>>,
    pub is_stencil: bool,
    pub has_color_key_mask: bool,
}

impl ImageInfo {
    pub fn class(&self) -> Class {
        if self.is_stencil
            || self.bpc == 1 && matches!(self.color, ColorSpace::Device(ColorModel::Gray))
        {
            Class::Bitonal
        } else {
            match self.color {
                ColorSpace::Indexed { .. } => Class::Indexed,
                ColorSpace::Device(ColorModel::Gray) => Class::Gray,
                _ => Class::Color,
            }
        }
    }

    /// Filter that produced the stored bytes, if it is an image codec.
    pub fn image_codec(&self) -> Option<&str> {
        self.filters.iter().map(String::as_str).find(|f| {
            matches!(
                *f,
                "DCTDecode" | "JPXDecode" | "CCITTFaxDecode" | "JBIG2Decode"
            )
        })
    }
}

pub fn read_info(doc: &Document, dict: &Dictionary) -> Option<ImageInfo> {
    let width = dict.get(b"Width").and_then(Object::as_i64).ok()?;
    let height = dict.get(b"Height").and_then(Object::as_i64).ok()?;
    if width <= 0 || height <= 0 || width > u32::MAX as i64 || height > u32::MAX as i64 {
        return None;
    }
    let is_stencil = dict
        .get(b"ImageMask")
        .and_then(Object::as_bool)
        .unwrap_or(false);
    let filters = filters(dict);
    // JPX codestreams carry their own depth and color space; the dictionary
    // may omit both.
    let jpx = filters.iter().any(|f| f == "JPXDecode");
    let bpc = if is_stencil {
        1
    } else {
        match dict.get(b"BitsPerComponent").and_then(Object::as_i64) {
            Ok(b) => b as u8,
            Err(_) if jpx => 8,
            Err(_) => return None,
        }
    };
    let color = if is_stencil {
        ColorSpace::Device(ColorModel::Gray)
    } else {
        match dict.get(b"ColorSpace") {
            Ok(cs) => color_space(doc, cs),
            Err(_) if jpx => ColorSpace::Other("from JPX codestream".into()),
            Err(_) => return None,
        }
    };
    Some(ImageInfo {
        width: width as u32,
        height: height as u32,
        bpc,
        color,
        filters,
        decode: decode_array(dict),
        is_stencil,
        has_color_key_mask: matches!(dict.get(b"Mask"), Ok(Object::Array(_))),
    })
}

fn filters(dict: &Dictionary) -> Vec<String> {
    match dict.get(b"Filter") {
        Ok(Object::Name(n)) => vec![String::from_utf8_lossy(n).into_owned()],
        Ok(Object::Array(items)) => items
            .iter()
            .filter_map(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect(),
        _ => Vec::new(),
    }
}

fn decode_array(dict: &Dictionary) -> Option<Vec<f32>> {
    let arr = dict.get(b"Decode").ok()?.as_array().ok()?;
    arr.iter().map(|o| o.as_float().ok()).collect()
}

fn color_space(doc: &Document, obj: &Object) -> ColorSpace {
    let obj = doc.dereference(obj).map(|(_, o)| o).unwrap_or(obj);
    match obj {
        Object::Name(n) => device_model(n).map_or_else(
            || ColorSpace::Other(String::from_utf8_lossy(n).into_owned()),
            ColorSpace::Device,
        ),
        Object::Array(items) => array_color_space(doc, items),
        _ => ColorSpace::Other("unresolvable".into()),
    }
}

fn array_color_space(doc: &Document, items: &[Object]) -> ColorSpace {
    let family = items.first().and_then(|o| o.as_name().ok()).unwrap_or(b"");
    match family {
        b"ICCBased" => icc_model(doc, items.get(1))
            .map_or_else(|| ColorSpace::Other("ICCBased".into()), ColorSpace::Device),
        b"Indexed" | b"I" => indexed(doc, items),
        b"CalRGB" => ColorSpace::Device(ColorModel::Rgb),
        b"CalGray" => ColorSpace::Device(ColorModel::Gray),
        other => ColorSpace::Other(String::from_utf8_lossy(other).into_owned()),
    }
}

fn icc_model(doc: &Document, stream: Option<&Object>) -> Option<ColorModel> {
    let (_, obj) = doc.dereference(stream?).ok()?;
    let n = obj
        .as_stream()
        .ok()?
        .dict
        .get(b"N")
        .and_then(Object::as_i64)
        .ok()?;
    match n {
        1 => Some(ColorModel::Gray),
        3 => Some(ColorModel::Rgb),
        4 => Some(ColorModel::Cmyk),
        _ => None,
    }
}

fn indexed(doc: &Document, items: &[Object]) -> ColorSpace {
    let base = items.get(1).map(|b| color_space(doc, b));
    let hival = items.get(2).and_then(|h| h.as_i64().ok()).unwrap_or(-1);
    match (base, hival) {
        (Some(ColorSpace::Device(base)), 0..=255) => ColorSpace::Indexed {
            base,
            hival: hival as u8,
        },
        _ => ColorSpace::Other("Indexed with unsupported base".into()),
    }
}

fn device_model(name: &[u8]) -> Option<ColorModel> {
    match name {
        b"DeviceGray" | b"G" | b"CalGray" => Some(ColorModel::Gray),
        b"DeviceRGB" | b"RGB" | b"CalRGB" => Some(ColorModel::Rgb),
        b"DeviceCMYK" | b"CMYK" => Some(ColorModel::Cmyk),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use lopdf::dictionary;

    use super::*;

    #[test]
    fn classes_follow_bits_and_color_space() {
        let doc = Document::with_version("1.5");
        let gray1 = read_info(&doc, &dictionary! { "Width" => 4, "Height" => 4, "BitsPerComponent" => 1, "ColorSpace" => "DeviceGray" }).unwrap();
        assert_eq!(gray1.class(), Class::Bitonal);
        let stencil = read_info(
            &doc,
            &dictionary! { "Width" => 4, "Height" => 4, "ImageMask" => true },
        )
        .unwrap();
        assert_eq!(stencil.class(), Class::Bitonal);
        let rgb = read_info(&doc, &dictionary! { "Width" => 4, "Height" => 4, "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB", "Filter" => "DCTDecode" }).unwrap();
        assert_eq!(rgb.class(), Class::Color);
        assert_eq!(rgb.image_codec(), Some("DCTDecode"));
        let idx = read_info(&doc, &dictionary! { "Width" => 4, "Height" => 4, "BitsPerComponent" => 8,
            "ColorSpace" => vec!["Indexed".into(), "DeviceRGB".into(), 15.into(), Object::string_literal("x")] }).unwrap();
        assert_eq!(idx.class(), Class::Indexed);
    }

    #[test]
    fn unknown_color_spaces_are_other() {
        let doc = Document::with_version("1.5");
        let info = read_info(
            &doc,
            &dictionary! { "Width" => 1, "Height" => 1, "BitsPerComponent" => 8,
            "ColorSpace" => vec!["Separation".into(), "Spot".into()] },
        )
        .unwrap();
        assert!(matches!(info.color, ColorSpace::Other(ref s) if s == "Separation"));
    }
}
