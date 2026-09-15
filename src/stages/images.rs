//! Stage 2: recompress image XObjects.
//!
//! Per image, in order:
//!   1. classify: bitonal / indexed / continuous (gray or color), or
//!      unsupported (route to "kept", note why);
//!   2. decode to a raster;
//!   3. transform: downsampling when effective DPI exceeds the class
//!      threshold (color conversion, complexity reduction and clipping are
//!      not implemented yet);
//!   4. encode with every codec allowed for the class and keep the
//!      smallest; the original bytes always compete;
//!   5. rewrite the stream and its dictionary, keeping the soft mask
//!      consistent.
//!
//! Invariant: an image never gets larger, and an image we cannot fully
//! understand is left byte-for-byte untouched.

mod bitonal;
mod classify;
mod decode;
mod encode;
mod raster;
mod transform;

use std::collections::HashSet;

use anyhow::Result;
use lopdf::{Document, Object, ObjectId, Stream};

use crate::config::{Codecs, ColorConversion, Config, Dpi};
use crate::pipeline::{Context, Stage};
use crate::report::ImageRow;
use classify::{Class, ImageInfo};
use decode::Skip;
use encode::Encoded;
use raster::{Format, Raster};

pub struct RecompressImages;

impl Stage for RecompressImages {
    fn name(&self) -> &'static str {
        "images"
    }

    fn enabled(&self, config: &Config) -> bool {
        !(config.bitonal.is_empty() && config.continuous.is_empty() && config.indexed.is_empty())
    }

    fn run(&self, doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        let masks = mask_ids(doc);
        let ids: Vec<ObjectId> = image_ids(doc)
            .into_iter()
            .filter(|id| !masks.contains(id))
            .collect();
        for id in ids {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                process_image(doc, id, ctx)
            }));
            if outcome.is_err() {
                ctx.report
                    .note(format!("image {} {}: kept: decoder panic", id.0, id.1));
            }
        }
        Ok(())
    }
}

fn image_ids(doc: &Document) -> Vec<ObjectId> {
    doc.objects
        .iter()
        .filter(|(_, o)| o.as_stream().is_ok_and(|s| is_image(&s.dict)))
        .map(|(&id, _)| id)
        .collect()
}

fn is_image(dict: &lopdf::Dictionary) -> bool {
    dict.get(b"Subtype")
        .and_then(Object::as_name)
        .is_ok_and(|s| s == b"Image")
}

/// Soft masks and stencil masks attached to other images: handled with
/// their parent, never on their own.
fn mask_ids(doc: &Document) -> HashSet<ObjectId> {
    doc.objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .filter(|s| is_image(&s.dict))
        .flat_map(|s| [s.dict.get(b"SMask"), s.dict.get(b"Mask")])
        .filter_map(|m| m.ok().and_then(|m| m.as_reference().ok()))
        .collect()
}

// --------------------------------------------------------------- one image

/// What happened to one image, for the report row.
struct Outcome {
    action: String,
    codec: Option<&'static str>,
    bytes_out: Option<usize>,
}

impl Outcome {
    fn kept(reason: impl Into<String>) -> Outcome {
        Outcome {
            action: format!("kept: {}", reason.into()),
            codec: None,
            bytes_out: None,
        }
    }
}

fn process_image(doc: &mut Document, id: ObjectId, ctx: &mut Context<'_>) {
    let Ok(Object::Stream(stream)) = doc.get_object(id) else {
        return;
    };
    let stream = stream.clone();
    let mut row = row_for(id, &stream);
    row.effective_dpi = ctx.usage.by_object.get(&id).and_then(|u| u.min_dpi());
    let outcome = match classify::read_info(doc, &stream.dict) {
        None => Outcome::kept("unreadable dictionary"),
        Some(info) => {
            let codecs = class_codecs(ctx.config, info.class());
            if codecs.is_empty() {
                Outcome::kept("class excluded")
            } else {
                let mask_note = if ctx.config.reduce_color_complexity {
                    simplify_soft_mask(doc, id)
                } else {
                    None
                };
                let task = Task {
                    id,
                    stream: &stream,
                    info,
                    config: ctx.config,
                    dpi: row.effective_dpi,
                };
                let mut outcome = attempt(doc, task);
                if let Some(note) = mask_note {
                    outcome.action = format!("{}+{note}", outcome.action);
                }
                outcome
            }
        }
    };
    row.action = outcome.action;
    row.filter_out = outcome.codec.map_or(row.filter_in.clone(), String::from);
    row.bytes_out = outcome.bytes_out.unwrap_or(row.bytes_in);
    ctx.report.images.push(row);
}

/// Everything `attempt` needs about one image.
struct Task<'a> {
    id: ObjectId,
    stream: &'a Stream,
    info: ImageInfo,
    config: &'a Config,
    dpi: Option<f32>,
}

fn attempt(doc: &mut Document, task: Task<'_>) -> Outcome {
    let prepared = match prepare(doc, &task) {
        Ok(p) => p,
        Err(Skip(reason)) => return Outcome::kept(reason),
    };
    let Some(best) = choose(&task, &prepared) else {
        return Outcome::kept("no encoder for this class yet");
    };
    if best.bytes.len() >= task.stream.content.len() {
        return Outcome::kept("source is smaller");
    }
    let (w, h) = (prepared.raster.width, prepared.raster.height);
    if prepared.resized && !resize_masks(doc, task.id, w, h) {
        return Outcome::kept("mask could not be resized");
    }
    write_back(doc, task.id, &prepared.raster, &best);
    if prepared.converted || prepared.reduced {
        set_color_space(doc, task.id, prepared.raster.format);
    }
    Outcome {
        action: prepared.action_label(),
        codec: Some(best.codec),
        bytes_out: Some(best.bytes.len()),
    }
}

/// The raster after decoding and the transforms the preset asks for.
struct Prepared {
    raster: Raster,
    reduced: bool,
    resized: bool,
    converted: bool,
}

fn prepare(doc: &Document, task: &Task<'_>) -> Result<Prepared, Skip> {
    let raster = decode::decode(doc, task.stream, &task.info)?;
    let (raster, reduced) = match reducible(&task.info, task.config) {
        true => match transform::reduce(&raster) {
            Some(r) => (r, true),
            None => (raster, false),
        },
        false => (raster, false),
    };
    let target = downsample_target(&raster, &task.info, task.config, task.dpi);
    let (raster, resized) = match target.and_then(|(w, h)| transform::downsample(&raster, w, h)) {
        Some(small) => (small, true),
        None => (raster, false),
    };
    let (raster, converted) = match convert_color(doc, &raster, &task.info, task.config) {
        Some(rgb) => (rgb, true),
        None => (raster, false),
    };
    Ok(Prepared {
        raster,
        reduced,
        resized,
        converted,
    })
}

/// Complexity reduction applies to plain device color spaces only: an ICC
/// profile, a color-key mask, or stencil semantics would be lost.
fn reducible(info: &ImageInfo, config: &Config) -> bool {
    config.reduce_color_complexity
        && info.icc_profile.is_none()
        && !info.is_stencil
        && !info.has_color_key_mask
        && matches!(info.color, classify::ColorSpace::Device(_))
}

/// The smallest candidate encoding, or `None` when no encoder applies.
fn choose(task: &Task<'_>, prepared: &Prepared) -> Option<Encoded> {
    let codecs = format_codecs(task.config, prepared.raster.format);
    let lossy_ok = !task.info.has_color_key_mask;
    let mut candidates = candidates(&prepared.raster, codecs, lossy_ok, task.config.jpeg_quality);
    if !prepared.resized
        && !prepared.converted
        && !prepared.reduced
        && task.info.image_codec() == Some("DCTDecode")
    {
        // The stored JPEG without any wrapper filters: lossless and often
        // smaller than the Flate-wrapped original.
        candidates.extend(passthrough(task.stream, &task.info));
    }
    candidates.into_iter().min_by_key(|e| e.bytes.len())
}

impl Prepared {
    fn action_label(&self) -> String {
        let steps: Vec<&str> = [
            (self.reduced, "reduced"),
            (self.resized, "downsampled"),
            (self.converted, "rgb"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        if steps.is_empty() {
            "recoded".into()
        } else {
            steps.join("+")
        }
    }
}

/// After a reduction or conversion the device space follows the raster.
fn set_color_space(doc: &mut Document, id: ObjectId, format: Format) {
    let name: &[u8] = match format {
        Format::Gray1 | Format::Gray8 => b"DeviceGray",
        Format::Rgb8 => b"DeviceRGB",
        Format::Cmyk8 => b"DeviceCMYK",
        Format::Indexed8 => return,
    };
    if let Ok(Object::Stream(s)) = doc.get_object_mut(id) {
        s.dict.set("ColorSpace", Object::Name(name.to_vec()));
    }
}

/// Codecs for the raster as it is now (reduction may have changed its
/// class since the dictionary was read).
fn format_codecs(config: &Config, format: Format) -> Codecs {
    match format {
        Format::Gray1 => config.bitonal,
        Format::Indexed8 => config.indexed,
        _ => config.continuous,
    }
}

/// RGB conversion when the preset asks for it: CMYK through the embedded
/// ICC profile when present, else the Neugebauer model; RGB with a profile
/// through the profile. Gray images stay gray (converting them would only
/// triple their size), and images with a color-key mask are never converted.
fn convert_color(
    doc: &Document,
    raster: &Raster,
    info: &ImageInfo,
    config: &Config,
) -> Option<Raster> {
    if config.color_conversion != ColorConversion::Rgb || info.has_color_key_mask {
        return None;
    }
    let profile = info.icc_profile.and_then(|id| match doc.get_object(id) {
        Ok(Object::Stream(s)) => s.decompressed_content_with_limit(16 << 20).ok(),
        _ => None,
    });
    match (raster.format, profile) {
        (Format::Cmyk8 | Format::Rgb8, Some(bytes)) => transform::icc_to_rgb(raster, &bytes),
        (Format::Cmyk8, None) => transform::cmyk_to_rgb(raster),
        _ => None,
    }
}

/// Target size when the class rule says to downsample. Images with a
/// color-key mask are never resampled (the key values would change).
fn downsample_target(
    raster: &Raster,
    info: &ImageInfo,
    config: &Config,
    dpi: Option<f32>,
) -> Option<(u32, u32)> {
    let rule = class_dpi(config, info.class());
    let dpi = dpi?;
    if !rule.enabled() || dpi <= rule.threshold || info.has_color_key_mask {
        return None;
    }
    Some(transform::scaled_size(
        raster.width,
        raster.height,
        rule.target / dpi,
    ))
}

fn candidates(raster: &Raster, codecs: Codecs, lossy_ok: bool, quality: u8) -> Vec<Encoded> {
    let mut out = Vec::new();
    if codecs.contains(Codecs::FLATE) {
        out.extend(encode::flate(raster));
    }
    if codecs.contains(Codecs::G4) {
        out.extend(bitonal::encode_g4(raster));
    }
    if codecs.contains(Codecs::JBIG2) {
        out.extend(bitonal::encode_jbig2(raster));
    }
    if codecs.contains(Codecs::JPEG)
        && lossy_ok
        && matches!(raster.format, Format::Gray8 | Format::Rgb8 | Format::Cmyk8)
    {
        out.extend(encode::jpeg(raster, quality));
    }
    out
}

fn passthrough(stream: &Stream, info: &ImageInfo) -> Option<Encoded> {
    let bytes = decode::codestream(stream, info).ok()?;
    Some(Encoded {
        codec: "DCTDecode",
        bytes,
        filter: Some(Object::Name(b"DCTDecode".to_vec())),
        decode_parms: None,
    })
}

fn row_for(id: ObjectId, stream: &Stream) -> ImageRow {
    let d = &stream.dict;
    let int = |k: &[u8]| d.get(k).and_then(Object::as_i64).unwrap_or(0);
    ImageRow {
        object: id,
        width: int(b"Width").max(0) as u32,
        height: int(b"Height").max(0) as u32,
        bits_per_component: int(b"BitsPerComponent").clamp(0, 16) as u8,
        color_space: describe(d.get(b"ColorSpace").ok()),
        filter_in: describe(d.get(b"Filter").ok()),
        effective_dpi: None,
        bytes_in: stream.content.len(),
        action: String::new(),
        filter_out: String::new(),
        bytes_out: stream.content.len(),
    }
}

fn describe(obj: Option<&Object>) -> String {
    match obj {
        Some(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
        Some(Object::Array(a)) => a
            .first()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_else(|| "array".into()),
        Some(Object::Reference(_)) => "ref".into(),
        Some(_) => "?".into(),
        None => "none".into(),
    }
}

fn class_codecs(config: &Config, class: Class) -> Codecs {
    match class {
        Class::Bitonal => config.bitonal,
        Class::Indexed => config.indexed,
        Class::Gray | Class::Color => config.continuous,
    }
}

fn class_dpi(config: &Config, class: Class) -> Dpi {
    match class {
        Class::Bitonal => config.bitonal_dpi,
        Class::Gray | Class::Indexed => config.gray_dpi,
        Class::Color => config.color_dpi,
    }
}

fn write_back(doc: &mut Document, id: ObjectId, raster: &Raster, best: &Encoded) {
    if let Ok(Object::Stream(s)) = doc.get_object_mut(id) {
        encode::apply(s, raster, best);
    }
}

// ------------------------------------------------------------------ masks
//
// Soft masks (`/SMask`) and stencil masks (`/Mask` referencing an image
// mask) attached to an image: simplification and resizing alongside the
// parent.

/// Simplify the parent's soft mask, when `reduce_color_complexity` asks:
/// an opaque mask is removed; a mask with only 0 and 255 becomes a
/// stencil `/Mask`. Returns what was done, for the report.
fn simplify_soft_mask(doc: &mut Document, parent_id: ObjectId) -> Option<&'static str> {
    let (mask_id, mask) = soft_mask(doc, parent_id)?;
    if mask.dict.has(b"Matte") {
        return None;
    }
    let info = classify::read_info(doc, &mask.dict)?;
    let raster = decode::decode(doc, &mask, &info).ok()?;
    if raster.format != Format::Gray8 {
        return None;
    }
    if raster.data.iter().all(|&v| v == 255) {
        parent_dict(doc, parent_id)?.remove(b"SMask");
        return Some("smask-removed");
    }
    if !raster.data.iter().all(|&v| v == 0 || v == 255) {
        return None;
    }
    // Soft mask 0 (transparent) is stencil 1 (masked out).
    let inverted: Vec<u8> = raster.data.iter().map(|&v| 255 - v).collect();
    let stencil =
        Raster::new(raster.width, raster.height, Format::Gray8, inverted)?.gray8_to_gray1();
    let best = best_bitonal(&stencil)?;
    let Ok(Object::Stream(s)) = doc.get_object_mut(mask_id) else {
        return None;
    };
    s.dict.set("ImageMask", true);
    s.dict.remove(b"ColorSpace");
    s.dict.remove(b"BitsPerComponent");
    encode::apply(s, &stencil, &best);
    let parent = parent_dict(doc, parent_id)?;
    parent.remove(b"SMask");
    parent.set("Mask", Object::Reference(mask_id));
    Some("smask-to-stencil")
}

/// Bring the parent's masks to `width` x `height` after the parent was
/// downsampled. Returns false when a mask exists but cannot be handled.
fn resize_masks(doc: &mut Document, parent_id: ObjectId, width: u32, height: u32) -> bool {
    let parent = match doc.get_object(parent_id) {
        Ok(Object::Stream(s)) => s.dict.clone(),
        _ => return false,
    };
    for key in [&b"SMask"[..], b"Mask"] {
        let Ok(Object::Reference(mask_id)) = parent.get(key) else {
            continue;
        };
        if !resize_one(doc, *mask_id, width, height) {
            return false;
        }
    }
    true
}

fn resize_one(doc: &mut Document, mask_id: ObjectId, width: u32, height: u32) -> bool {
    let Ok(Object::Stream(mask)) = doc.get_object(mask_id) else {
        return false;
    };
    let mask = mask.clone();
    let Some(small) = classify::read_info(doc, &mask.dict)
        .and_then(|info| decode::decode(doc, &mask, &info).ok())
        .and_then(|raster| transform::downsample(&raster, width, height))
    else {
        return false;
    };
    let best = match small.format {
        Format::Gray1 => best_bitonal(&small),
        _ => encode::flate(&small),
    };
    let Some(best) = best else {
        return false;
    };
    if let Ok(Object::Stream(s)) = doc.get_object_mut(mask_id) {
        encode::apply(s, &small, &best);
    }
    true
}

fn best_bitonal(raster: &Raster) -> Option<Encoded> {
    [encode::flate(raster), bitonal::encode_g4(raster)]
        .into_iter()
        .flatten()
        .min_by_key(|e| e.bytes.len())
}

fn soft_mask(doc: &Document, parent_id: ObjectId) -> Option<(ObjectId, Stream)> {
    let Ok(Object::Stream(parent)) = doc.get_object(parent_id) else {
        return None;
    };
    let mask_id = parent.dict.get(b"SMask").ok()?.as_reference().ok()?;
    match doc.get_object(mask_id) {
        Ok(Object::Stream(s)) => Some((mask_id, s.clone())),
        _ => None,
    }
}

fn parent_dict(doc: &mut Document, parent_id: ObjectId) -> Option<&mut lopdf::Dictionary> {
    match doc.get_object_mut(parent_id) {
        Ok(Object::Stream(s)) => Some(&mut s.dict),
        _ => None,
    }
}

#[cfg(test)]
mod mask_tests {
    use lopdf::dictionary;

    use super::*;
    use lopdf::Stream;

    fn doc_with_mask(mask_pixels: Vec<u8>) -> (Document, ObjectId, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let mask = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 2, "Height" => 2,
            "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
            mask_pixels,
        ));
        let parent = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 2, "Height" => 2,
            "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8, "SMask" => mask },
            vec![0; 12],
        ));
        (doc, parent, mask)
    }

    #[test]
    fn opaque_soft_mask_is_removed() {
        let (mut doc, parent, _) = doc_with_mask(vec![255; 4]);
        assert_eq!(simplify_soft_mask(&mut doc, parent), Some("smask-removed"));
        let parent = doc.get_object(parent).unwrap().as_stream().unwrap();
        assert!(!parent.dict.has(b"SMask"));
    }

    #[test]
    fn two_level_soft_mask_becomes_a_stencil() {
        let (mut doc, parent, mask) = doc_with_mask(vec![0, 255, 255, 0]);
        assert_eq!(
            simplify_soft_mask(&mut doc, parent),
            Some("smask-to-stencil")
        );
        let parent = doc.get_object(parent).unwrap().as_stream().unwrap();
        assert_eq!(
            parent.dict.get(b"Mask").unwrap().as_reference().unwrap(),
            mask
        );
        let mask = doc.get_object(mask).unwrap().as_stream().unwrap();
        assert!(mask.dict.get(b"ImageMask").unwrap().as_bool().unwrap());
        // Transparent (0) pixels are masked out (1): row 0 = 10, row 1 = 01.
        let info = classify::read_info(&doc, &mask.dict).unwrap();
        let bits = decode::decode(&doc, mask, &info).unwrap();
        assert_eq!(bits.format, Format::Gray1);
        assert_eq!(bits.data, vec![0b1000_0000, 0b0100_0000]);
    }

    #[test]
    fn graded_soft_mask_is_left_alone() {
        let (mut doc, parent, _) = doc_with_mask(vec![0, 100, 200, 255]);
        assert_eq!(simplify_soft_mask(&mut doc, parent), None);
    }
}
