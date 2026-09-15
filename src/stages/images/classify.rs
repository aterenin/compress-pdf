//! Read an image XObject's dictionary into a plain description and decide
//! which class (bitonal, indexed, continuous) it belongs to. The second
//! half of the file handles color spaces whose samples are not device
//! values (Separation, DeviceN through their tint transform, and Lab): a
//! [`Mapping`] turns one tuple of decoded component values into device
//! samples of its [`ColorModel`], which is what the rest of the stage
//! works in.

use std::collections::HashMap;

use lopdf::{Dictionary, Document, Object};

use super::function::Function;

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
    /// `palette` is set when the base was a mapped space: the palette
    /// converted to `base`, which the dictionary must then be given.
    Indexed {
        base: ColorModel,
        hival: u8,
        palette: Option<Vec<u8>>,
    },
    /// Separation, DeviceN or Lab: `components` samples per pixel that a
    /// [`Mapping`] built from `source` turns into `model`.
    Mapped {
        components: usize,
        model: ColorModel,
        source: Object,
    },
    /// Anything the stage does not handle (Pattern, unresolvable...).
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
    /// The ICCBased profile stream, when the color space has one.
    pub icc_profile: Option<lopdf::ObjectId>,
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
                ColorSpace::Device(ColorModel::Gray)
                | ColorSpace::Mapped {
                    model: ColorModel::Gray,
                    ..
                } => Class::Gray,
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
    let filters = filters(doc, dict);
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
        icc_profile: icc_profile_id(doc, dict),
    })
}

fn icc_profile_id(doc: &Document, dict: &Dictionary) -> Option<lopdf::ObjectId> {
    let cs = dict.get(b"ColorSpace").ok()?;
    let cs = doc.dereference(cs).map(|(_, o)| o).unwrap_or(cs);
    let items = cs.as_array().ok()?;
    if items.first()?.as_name().ok()? != b"ICCBased" {
        return None;
    }
    items.get(1)?.as_reference().ok()
}

/// Filter names; the entry and its elements may be indirect.
fn filters(doc: &Document, dict: &Dictionary) -> Vec<String> {
    let Ok(filter) = dict.get(b"Filter") else {
        return Vec::new();
    };
    let deref = |o: &Object| {
        doc.dereference(o)
            .map(|(_, o)| o.clone())
            .unwrap_or_else(|_| o.clone())
    };
    match deref(filter) {
        Object::Name(n) => vec![String::from_utf8_lossy(&n).into_owned()],
        Object::Array(items) => items
            .iter()
            .filter_map(|o| {
                deref(o)
                    .as_name()
                    .ok()
                    .map(|n| String::from_utf8_lossy(n).into_owned())
            })
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
        b"Separation" | b"DeviceN" | b"Lab" => mapped(doc, items),
        // A bare family name in a one-element array.
        _ if items.len() == 1 => color_space(doc, &items[0]),
        other => ColorSpace::Other(String::from_utf8_lossy(other).into_owned()),
    }
}

/// Separation and DeviceN take the model of their alternate space; Lab
/// converts to RGB. The tint transform itself is parsed at decode time.
fn mapped(doc: &Document, items: &[Object]) -> ColorSpace {
    let family = items.first().and_then(|o| o.as_name().ok()).unwrap_or(b"");
    let (components, model) = match family {
        b"Lab" => (Some(3), Some(ColorModel::Rgb)),
        _ => {
            let names = items
                .get(1)
                .map(|n| doc.dereference(n).map(|(_, o)| o).unwrap_or(n));
            let components = match family {
                b"Separation" => Some(1),
                _ => names.and_then(|n| n.as_array().ok()).map(Vec::len),
            };
            let model = items
                .get(2)
                .map(|a| color_space(doc, a))
                .and_then(|cs| match cs {
                    ColorSpace::Device(m) | ColorSpace::Mapped { model: m, .. } => Some(m),
                    _ => None,
                });
            (components, model)
        }
    };
    match (components, model) {
        (Some(components), Some(model)) if components > 0 => ColorSpace::Mapped {
            components,
            model,
            source: Object::Array(items.to_vec()),
        },
        _ => ColorSpace::Other(format!(
            "{} with an unsupported alternate",
            String::from_utf8_lossy(family)
        )),
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

pub fn components(model: ColorModel) -> usize {
    match model {
        ColorModel::Gray => 1,
        ColorModel::Rgb => 3,
        ColorModel::Cmyk => 4,
    }
}

fn indexed(doc: &Document, items: &[Object]) -> ColorSpace {
    let base = items.get(1).map(|b| color_space(doc, b));
    let hival = items.get(2).and_then(|h| h.as_i64().ok()).unwrap_or(-1);
    match (base, hival) {
        (Some(ColorSpace::Device(base)), 0..=255) => ColorSpace::Indexed {
            base,
            hival: hival as u8,
            palette: None,
        },
        (Some(ColorSpace::Mapped { model, source, .. }), 0..=255) => {
            match mapped_palette(doc, items.get(3), &source, hival as usize + 1) {
                Some(palette) => ColorSpace::Indexed {
                    base: model,
                    hival: hival as u8,
                    palette: Some(palette),
                },
                None => ColorSpace::Other("Indexed palette does not map".into()),
            }
        }
        _ => ColorSpace::Other("Indexed with unsupported base".into()),
    }
}

/// Run a palette (string or stream) through a mapped base space so the
/// image can be described in the base's device model.
fn mapped_palette(
    doc: &Document,
    lookup: Option<&Object>,
    source: &Object,
    entries: usize,
) -> Option<Vec<u8>> {
    let lookup = doc.dereference(lookup?).map(|(_, o)| o).ok()?;
    let bytes = match lookup {
        Object::String(s, _) => s.clone(),
        Object::Stream(s) => s.decompressed_content_with_limit(1 << 20).ok()?,
        _ => return None,
    };
    let mapping = Mapping::build(doc, source)?;
    let n = mapping.components;
    if bytes.len() < entries * n {
        return None;
    }
    let mut memo = Memo::new(&mapping, mapping.default_decode(), 8);
    let mut out = Vec::with_capacity(entries * components(mapping.model));
    let mut tuple = vec![0u16; n];
    for entry in bytes[..entries * n].chunks(n) {
        for (t, b) in tuple.iter_mut().zip(entry) {
            *t = u16::from(*b);
        }
        out.extend_from_slice(memo.lookup(&tuple)?);
    }
    Some(out)
}

fn device_model(name: &[u8]) -> Option<ColorModel> {
    match name {
        b"DeviceGray" | b"G" | b"CalGray" => Some(ColorModel::Gray),
        b"DeviceRGB" | b"RGB" | b"CalRGB" => Some(ColorModel::Rgb),
        b"DeviceCMYK" | b"CMYK" => Some(ColorModel::Cmyk),
        _ => None,
    }
}

// ------------------------------------------------------ mapped spaces

#[derive(Debug, Clone, PartialEq)]
pub struct Mapping {
    pub components: usize,
    pub model: ColorModel,
    kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Device,
    Lab {
        white: [f32; 3],
        range: [f32; 4],
    },
    Tint {
        function: Function,
        alternate: Box<Mapping>,
    },
}

impl Mapping {
    /// Build from a color space object (name or array, possibly indirect).
    pub fn build(doc: &Document, cs: &Object) -> Option<Mapping> {
        Self::build_depth(doc, cs, 0)
    }

    fn build_depth(doc: &Document, cs: &Object, depth: usize) -> Option<Mapping> {
        if depth > 4 {
            return None;
        }
        let cs = doc.dereference(cs).map(|(_, o)| o).unwrap_or(cs);
        match cs {
            Object::Name(n) => Some(Mapping::device(device_model(n)?)),
            Object::Array(items) => Mapping::array(doc, items, depth),
            _ => None,
        }
    }

    fn array(doc: &Document, items: &[Object], depth: usize) -> Option<Mapping> {
        match items.first()?.as_name().ok()? {
            b"Separation" => tint(doc, items, 1, depth),
            b"DeviceN" => {
                let names = items.get(1)?;
                let names = doc.dereference(names).map(|(_, o)| o).unwrap_or(names);
                tint(doc, items, names.as_array().ok()?.len(), depth)
            }
            b"Lab" => lab(doc, items.get(1)?),
            b"ICCBased" => Some(Mapping::device(icc_model(doc, items.get(1))?)),
            b"CalRGB" => Some(Mapping::device(ColorModel::Rgb)),
            b"CalGray" => Some(Mapping::device(ColorModel::Gray)),
            _ => None,
        }
    }

    fn device(model: ColorModel) -> Mapping {
        Mapping {
            components: components(model),
            model,
            kind: Kind::Device,
        }
    }

    /// The `Decode` array that leaves samples unchanged: 0..1 per
    /// component, except Lab whose natural ranges are L 0..100 and the
    /// dictionary's `Range` for a and b.
    pub fn default_decode(&self) -> Vec<f32> {
        match &self.kind {
            Kind::Lab { range, .. } => vec![0.0, 100.0, range[0], range[1], range[2], range[3]],
            _ => (0..self.components).flat_map(|_| [0.0, 1.0]).collect(),
        }
    }

    /// Device samples (0..1, `components(model)` of them) for one input
    /// tuple in the space's natural ranges.
    pub fn apply(&self, input: &[f32], out: &mut Vec<f32>) -> Option<()> {
        out.clear();
        match &self.kind {
            Kind::Device => out.extend(
                input
                    .iter()
                    .take(self.components)
                    .map(|v| v.clamp(0.0, 1.0)),
            ),
            Kind::Lab { white, range } => out.extend(lab_to_srgb(input, *white, *range)?),
            Kind::Tint {
                function,
                alternate,
            } => {
                let values = function.eval(input)?;
                if values.len() < alternate.components {
                    return None;
                }
                let mut inner = Vec::with_capacity(4);
                alternate.apply(&values, &mut inner)?;
                out.extend(inner);
            }
        }
        Some(())
    }
}

/// Memoized per-tuple evaluation over quantized samples: maps each
/// distinct input tuple once, which is what makes DeviceN photos and
/// Separation scans affordable.
pub struct Memo<'a> {
    mapping: &'a Mapping,
    decode: Vec<f32>,
    max: f32,
    cache: HashMap<Vec<u16>, Vec<u8>>,
    input: Vec<f32>,
    output: Vec<f32>,
}

impl<'a> Memo<'a> {
    /// `decode` is the image's `Decode` array (or the default), `bpc` the
    /// depth the quantized samples are in.
    pub fn new(mapping: &'a Mapping, decode: Vec<f32>, bpc: u8) -> Memo<'a> {
        Memo {
            mapping,
            decode,
            max: ((1u32 << bpc) - 1) as f32,
            cache: HashMap::new(),
            input: Vec::with_capacity(mapping.components),
            output: Vec::with_capacity(4),
        }
    }

    /// Device samples, one byte each, for one pixel's quantized samples.
    pub fn lookup(&mut self, samples: &[u16]) -> Option<&[u8]> {
        if !self.cache.contains_key(samples) {
            let bytes = self.compute(samples)?;
            self.cache.insert(samples.to_vec(), bytes);
        }
        self.cache.get(samples).map(Vec::as_slice)
    }

    fn compute(&mut self, samples: &[u16]) -> Option<Vec<u8>> {
        self.input.clear();
        for (i, s) in samples.iter().enumerate() {
            let (dmin, dmax) = (self.decode.get(2 * i)?, self.decode.get(2 * i + 1)?);
            self.input
                .push(dmin + f32::from(*s) / self.max * (dmax - dmin));
        }
        self.mapping.apply(&self.input, &mut self.output)?;
        Some(
            self.output
                .iter()
                .map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
                .collect(),
        )
    }
}

fn tint(doc: &Document, items: &[Object], n: usize, depth: usize) -> Option<Mapping> {
    let alternate = Mapping::build_depth(doc, items.get(2)?, depth + 1)?;
    let function = Function::parse(doc, items.get(3)?)?;
    if function.outputs().is_some_and(|o| o < alternate.components) {
        return None;
    }
    Some(Mapping {
        components: n,
        model: alternate.model,
        kind: Kind::Tint {
            function,
            alternate: Box::new(alternate),
        },
    })
}

fn lab(doc: &Document, dict: &Object) -> Option<Mapping> {
    let dict = doc.dereference(dict).map(|(_, o)| o).unwrap_or(dict);
    let dict = dict.as_dict().ok()?;
    let floats = |key: &[u8]| -> Option<Vec<f32>> {
        dict.get(key)
            .ok()?
            .as_array()
            .ok()?
            .iter()
            .map(|o| o.as_float().ok())
            .collect()
    };
    let white = floats(b"WhitePoint").unwrap_or_else(|| vec![0.9505, 1.0, 1.089]);
    let range = floats(b"Range").unwrap_or_else(|| vec![-100.0, 100.0, -100.0, 100.0]);
    if white.len() != 3 || range.len() != 4 {
        return None;
    }
    Some(Mapping {
        components: 3,
        model: ColorModel::Rgb,
        kind: Kind::Lab {
            white: [white[0], white[1], white[2]],
            range: [range[0], range[1], range[2], range[3]],
        },
    })
}

/// CIE L*a*b* to sRGB: Lab to XYZ under the space's white point, Bradford
/// adaptation to D65, the sRGB matrix, and the sRGB transfer curve.
fn lab_to_srgb(input: &[f32], white: [f32; 3], range: [f32; 4]) -> Option<[f32; 3]> {
    let l = input.first()?.clamp(0.0, 100.0);
    let a = input.get(1)?.clamp(range[0], range[1]);
    let b = input.get(2)?.clamp(range[2], range[3]);
    let m = (l + 16.0) / 116.0;
    let g = |t: f32| {
        if t >= 6.0 / 29.0 {
            t * t * t
        } else {
            108.0 / 841.0 * (t - 4.0 / 29.0)
        }
    };
    let xyz = [
        white[0] * g(m + a / 500.0),
        white[1] * g(m),
        white[2] * g(m - b / 200.0),
    ];
    let rgb = mul3(SRGB_FROM_XYZ, adapt_to_d65(xyz, white));
    Some(rgb.map(|c| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.003_130_8 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    }))
}

const D65: [f32; 3] = [0.9505, 1.0, 1.089];
const BRADFORD: [[f32; 3]; 3] = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];
const BRADFORD_INV: [[f32; 3]; 3] = [
    [0.9870, -0.1471, 0.1600],
    [0.4323, 0.5184, 0.0493],
    [-0.0085, 0.0400, 0.9685],
];
const SRGB_FROM_XYZ: [[f32; 3]; 3] = [
    [3.2406, -1.5372, -0.4986],
    [-0.9689, 1.8758, 0.0415],
    [0.0557, -0.2040, 1.0570],
];

fn mul3(m: [[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
}

/// Bradford chromatic adaptation from `white` to D65; the identity when
/// the white point already is D65.
fn adapt_to_d65(xyz: [f32; 3], white: [f32; 3]) -> [f32; 3] {
    let src = mul3(BRADFORD, white);
    let dst = mul3(BRADFORD, D65);
    let cone = mul3(BRADFORD, xyz);
    let scaled = [0, 1, 2].map(|i| {
        if src[i] == 0.0 {
            0.0
        } else {
            cone[i] * dst[i] / src[i]
        }
    });
    mul3(BRADFORD_INV, scaled)
}

#[cfg(test)]
mod mapping_tests {
    use lopdf::{Stream, dictionary};

    use super::*;

    fn separation_to_cmyk(doc: &mut Document) -> Object {
        // A spot color that is 80% cyan, 20% black at full tint.
        let f = doc.add_object(Stream::new(
            dictionary! { "FunctionType" => 4, "Domain" => vec![0.into(), 1.into()],
                "Range" => vec![0.into(), 1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into(), 1.into()] },
            b"{ dup 0.8 mul exch 0.2 mul 0 0 3 -1 roll }".to_vec(),
        ));
        Object::Array(vec![
            "Separation".into(),
            "Spot".into(),
            "DeviceCMYK".into(),
            f.into(),
        ])
    }

    #[test]
    fn separation_maps_through_the_tint_transform() {
        let mut doc = Document::with_version("1.5");
        let cs = separation_to_cmyk(&mut doc);
        let m = Mapping::build(&doc, &cs).unwrap();
        assert_eq!((m.components, m.model), (1, ColorModel::Cmyk));
        let mut memo = Memo::new(&m, m.default_decode(), 8);
        assert_eq!(memo.lookup(&[255]).unwrap(), &[204, 0, 0, 51]);
        assert_eq!(memo.lookup(&[0]).unwrap(), &[0, 0, 0, 0]);
        // Same tuple again comes from the cache.
        assert_eq!(memo.lookup(&[255]).unwrap(), &[204, 0, 0, 51]);
        assert_eq!(memo.cache.len(), 2);
    }

    #[test]
    fn devicen_with_two_inks_to_gray() {
        let doc = Document::with_version("1.5");
        let f = Object::Stream(Stream::new(
            dictionary! { "FunctionType" => 4, "Domain" => vec![0.into(), 1.into(), 0.into(), 1.into()],
            "Range" => vec![0.into(), 1.into()] },
            b"{ add 2 div 1 exch sub }".to_vec(),
        ));
        let cs = Object::Array(vec![
            "DeviceN".into(),
            Object::Array(vec!["A".into(), "B".into()]),
            "DeviceGray".into(),
            f,
        ]);
        let m = Mapping::build(&doc, &cs).unwrap();
        assert_eq!((m.components, m.model), (2, ColorModel::Gray));
        let mut memo = Memo::new(&m, m.default_decode(), 4);
        assert_eq!(memo.lookup(&[15, 15]).unwrap(), &[0]);
        assert_eq!(memo.lookup(&[0, 0]).unwrap(), &[255]);
    }

    #[test]
    fn lab_white_black_and_red() {
        let doc = Document::with_version("1.5");
        let cs = Object::Array(vec![
            "Lab".into(),
            Object::Dictionary(
                dictionary! { "WhitePoint" => vec![0.9505.into(), 1.0.into(), 1.089.into()],
                "Range" => vec![(-128).into(), 127.into(), (-128).into(), 127.into()] },
            ),
        ]);
        let m = Mapping::build(&doc, &cs).unwrap();
        assert_eq!(
            m.default_decode(),
            vec![0.0, 100.0, -128.0, 127.0, -128.0, 127.0]
        );
        let mut memo = Memo::new(&m, m.default_decode(), 8);
        assert_eq!(memo.lookup(&[255, 128, 128]).unwrap(), &[255, 255, 255]);
        assert_eq!(memo.lookup(&[0, 128, 128]).unwrap(), &[0, 0, 0]);
        // L 54, a 81, b 70 is sRGB red.
        let red = memo.lookup(&[138, 209, 198]).unwrap().to_vec();
        assert!(red[0] > 230 && red[1] < 40 && red[2] < 40, "{red:?}");
    }

    #[test]
    fn unsupported_spaces_are_none() {
        let doc = Document::with_version("1.5");
        assert!(Mapping::build(&doc, &Object::Name(b"Pattern".to_vec())).is_none());
        let cs = Object::Array(vec![
            "Separation".into(),
            "X".into(),
            "Pattern".into(),
            1.into(),
        ]);
        assert!(Mapping::build(&doc, &cs).is_none());
    }
}

#[cfg(test)]
mod tests {
    use lopdf::{Stream, dictionary};

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
            "ColorSpace" => vec!["Pattern".into()] },
        )
        .unwrap();
        assert!(matches!(info.color, ColorSpace::Other(ref s) if s == "Pattern"));
        let info = read_info(
            &doc,
            &dictionary! { "Width" => 1, "Height" => 1, "BitsPerComponent" => 8,
            "ColorSpace" => vec!["Separation".into(), "Spot".into(), "Pattern".into(), 1.into()] },
        )
        .unwrap();
        assert!(matches!(info.color, ColorSpace::Other(_)));
    }

    #[test]
    fn indexed_over_a_mapped_base_gets_a_device_palette() {
        let mut doc = Document::with_version("1.5");
        // Tint 1 is full black in the DeviceGray alternate (1 - x).
        let f = doc.add_object(Stream::new(
            dictionary! { "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()],
            "C0" => vec![1.into()], "C1" => vec![0.into()], "N" => 1 },
            vec![],
        ));
        let sep: Object = vec![
            "Separation".into(),
            "Spot".into(),
            "DeviceGray".into(),
            f.into(),
        ]
        .into();
        let info = read_info(
            &doc,
            &dictionary! { "Width" => 1, "Height" => 1, "BitsPerComponent" => 8,
            "ColorSpace" => vec!["Indexed".into(), sep, 2.into(), Object::string_literal(vec![0u8, 128, 255])] },
        )
        .unwrap();
        assert_eq!(info.class(), Class::Indexed);
        let ColorSpace::Indexed {
            base,
            hival,
            palette,
        } = info.color
        else {
            panic!("not indexed");
        };
        assert_eq!((base, hival), (ColorModel::Gray, 2));
        assert_eq!(palette, Some(vec![255, 127, 0]));
    }

    #[test]
    fn indirect_filters_and_wrapped_names_resolve() {
        let mut doc = Document::with_version("1.5");
        let filter = doc.add_object(Object::Name(b"DCTDecode".to_vec()));
        let info = read_info(
            &doc,
            &dictionary! { "Width" => 1, "Height" => 1, "BitsPerComponent" => 8,
            "ColorSpace" => vec!["DeviceRGB".into()], "Filter" => filter },
        )
        .unwrap();
        assert_eq!(info.color, ColorSpace::Device(ColorModel::Rgb));
        assert_eq!(info.image_codec(), Some("DCTDecode"));
    }

    #[test]
    fn mapped_spaces_take_the_alternate_model() {
        let doc = Document::with_version("1.5");
        let sep = read_info(
            &doc,
            &dictionary! { "Width" => 1, "Height" => 1, "BitsPerComponent" => 8,
            "ColorSpace" => vec!["Separation".into(), "Spot".into(), "DeviceGray".into(), 1.into()] },
        )
        .unwrap();
        assert!(matches!(
            sep.color,
            ColorSpace::Mapped {
                components: 1,
                model: ColorModel::Gray,
                ..
            }
        ));
        assert_eq!(sep.class(), Class::Gray);
        let devn = read_info(
            &doc,
            &dictionary! { "Width" => 1, "Height" => 1, "BitsPerComponent" => 8,
            "ColorSpace" => vec!["DeviceN".into(), vec!["A".into(), "B".into()].into(), "DeviceCMYK".into(), 1.into()] },
        )
        .unwrap();
        assert!(matches!(
            devn.color,
            ColorSpace::Mapped {
                components: 2,
                model: ColorModel::Cmyk,
                ..
            }
        ));
        assert_eq!(devn.class(), Class::Color);
        let lab = read_info(
            &doc,
            &dictionary! { "Width" => 1, "Height" => 1, "BitsPerComponent" => 8,
            "ColorSpace" => vec!["Lab".into(), dictionary! {}.into()] },
        )
        .unwrap();
        assert!(matches!(
            lab.color,
            ColorSpace::Mapped {
                components: 3,
                model: ColorModel::Rgb,
                ..
            }
        ));
    }
}
