//! Remove resource dictionary entries that no content stream refers to.
//!
//! Owners of a `/Resources` dictionary are pages, form XObjects (including
//! annotation appearance streams), tiling pattern streams, and Type 3 fonts.
//! Each owner's content is parsed and the names it uses per category are
//! collected; a resource dictionary shared by several owners keeps the
//! union. Anything not fully understood is left alone: owners whose content
//! does not parse, resources inherited from the page tree, and Type 3 fonts
//! without their own resources (their glyph procedures draw with the page's).
//! Inline images may name a color space resource, so the ColorSpace category
//! is kept whole for any owner that contains one.

use std::collections::{HashMap, HashSet};

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId};

/// Streams larger than this are not parsed; their owners are left alone.
const MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;

const CATEGORIES: [&[u8]; 7] = [
    b"ExtGState",
    b"ColorSpace",
    b"Pattern",
    b"Shading",
    b"XObject",
    b"Font",
    b"Properties",
];

/// Where a `/Resources` dictionary lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ResLoc {
    /// An indirect object, possibly shared by several owners.
    Indirect(ObjectId),
    /// Inline in the owner's dictionary.
    InlineIn(ObjectId),
}

/// Where one category dictionary (e.g. `/Font`) lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CatLoc {
    Indirect(ObjectId),
    InlineIn(ResLoc, usize),
}

#[derive(Default)]
struct Usage {
    /// Names used per category index (into CATEGORIES).
    names: [HashSet<Vec<u8>>; 7],
    /// Categories that must be kept whole for this owner.
    keep_all: [bool; 7],
}

/// Fonts in an AcroForm's default resources (`/DR`) that no default
/// appearance string names. Nothing else can select them: fields draw
/// with the fonts their `/DA` strings name, and an XFA engine (which may
/// pick fonts by name from the same dictionary) is ruled out by requiring
/// that the document has no `/XFA` entry. Returns the number removed.
pub fn prune_default_resource_fonts(doc: &mut Document, used: &HashSet<Vec<u8>>) -> usize {
    let Some((at, doomed)) = default_resource_fonts_to_drop(doc, used) else {
        return 0;
    };
    let Some(fonts) = dict_at_mut(doc, &at) else {
        return 0;
    };
    for name in &doomed {
        fonts.remove(name);
    }
    doomed.len()
}

/// A dictionary reached from an object by a chain of inline keys.
struct DictPath {
    base: ObjectId,
    keys: Vec<Vec<u8>>,
}

fn default_resource_fonts_to_drop(
    doc: &Document,
    used: &HashSet<Vec<u8>>,
) -> Option<(DictPath, Vec<Vec<u8>>)> {
    let root = doc.trailer.get(b"Root").ok()?.as_reference().ok()?;
    let mut at = DictPath {
        base: root,
        keys: Vec::new(),
    };
    for key in [&b"AcroForm"[..], b"DR", b"Font"] {
        let dict = dict_at(doc, &at)?;
        if key == b"DR" && dict.has(b"XFA") {
            return None;
        }
        match dict.get(key).ok()? {
            Object::Reference(id) => {
                at = DictPath {
                    base: *id,
                    keys: Vec::new(),
                }
            }
            Object::Dictionary(_) => at.keys.push(key.to_vec()),
            _ => return None,
        }
    }
    let doomed: Vec<Vec<u8>> = dict_at(doc, &at)?
        .iter()
        .filter(|(name, _)| !used.contains(*name))
        .map(|(name, _)| name.clone())
        .collect();
    Some((at, doomed))
}

fn dict_at<'a>(doc: &'a Document, at: &DictPath) -> Option<&'a Dictionary> {
    let mut dict = doc.get_dictionary(at.base).ok()?;
    for key in &at.keys {
        dict = dict.get(key).ok()?.as_dict().ok()?;
    }
    Some(dict)
}

fn dict_at_mut<'a>(doc: &'a mut Document, at: &DictPath) -> Option<&'a mut Dictionary> {
    let mut dict = doc.get_dictionary_mut(at.base).ok()?;
    for key in &at.keys {
        dict = dict.get_mut(key).ok()?.as_dict_mut().ok()?;
    }
    Some(dict)
}

/// Returns the number of entries removed.
pub fn prune_unused(doc: &mut Document) -> usize {
    let protected = inherited_resources(doc);
    let mut usage: HashMap<ResLoc, Usage> = HashMap::new();
    let mut skipped: HashSet<ResLoc> = protected.clone();
    for (loc, content) in owners(doc) {
        match parse_usage(&content) {
            Some(u) => merge_usage(usage.entry(loc).or_default(), u),
            None => {
                skipped.insert(loc);
            }
        }
    }
    let plan = plan_removals(doc, &usage, &skipped);
    apply_removals(doc, plan)
}

/// Resources referenced from page-tree nodes are inherited by pages we do
/// not analyze through them; leave them untouched.
fn inherited_resources(doc: &Document) -> HashSet<ResLoc> {
    doc.objects
        .values()
        .filter_map(|o| o.as_dict().ok())
        .filter(|d| {
            d.get(b"Type")
                .and_then(Object::as_name)
                .is_ok_and(|t| t == b"Pages")
        })
        .filter_map(|d| d.get(b"Resources").ok())
        .filter_map(|r| r.as_reference().ok())
        .map(ResLoc::Indirect)
        .collect()
}

/// Every (resources location, content bytes) pair to analyze.
fn owners(doc: &Document) -> Vec<(ResLoc, Vec<u8>)> {
    let mut out = Vec::new();
    let type3_without_resources = type3_fonts_without_resources(doc);
    for page_id in doc.page_iter() {
        let Some(loc) = page_resources(doc, page_id, &type3_without_resources) else {
            continue;
        };
        // A page whose content cannot be decoded is left alone (an empty
        // usage set would strip everything); it is simply not an owner.
        if let Some(content) = page_content(doc, page_id) {
            out.push((loc, content));
        }
    }
    for (&id, obj) in &doc.objects {
        match obj {
            Object::Stream(s) if is_form_or_pattern(&s.dict) => {
                if let (Some(loc), Ok(content)) = (
                    res_loc(&s.dict, id),
                    s.decompressed_content_with_limit(MAX_CONTENT_BYTES),
                ) {
                    out.push((loc, content));
                }
            }
            Object::Dictionary(d) if is_type3(d) => {
                if let Some(loc) = res_loc(d, id) {
                    out.push((loc, charprocs_content(doc, d)));
                }
            }
            _ => {}
        }
    }
    out
}

/// All content streams of a page, concatenated; `None` if any of them fails
/// to decode. lopdf's own page-content helper swallows such failures and
/// returns what it could, which is not safe here.
fn page_content(doc: &Document, page_id: ObjectId) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for id in doc.get_page_contents(page_id) {
        let Ok(Object::Stream(stream)) = doc.get_object(id) else {
            return None;
        };
        out.extend(
            stream
                .decompressed_content_with_limit(MAX_CONTENT_BYTES)
                .ok()?,
        );
        out.push(b'\n');
    }
    Some(out)
}

fn page_resources(
    doc: &Document,
    page_id: ObjectId,
    type3_without_resources: &HashSet<ObjectId>,
) -> Option<ResLoc> {
    let page = doc.get_dictionary(page_id).ok()?;
    let loc = res_loc(page, page_id)?;
    // A Type 3 font without resources draws its glyphs with this page's
    // resources; we do not parse glyph procedures in that context.
    let fonts = resources_dict(doc, loc)?.get(b"Font").ok()?;
    let fonts = match fonts {
        Object::Reference(id) => doc.get_dictionary(*id).ok()?,
        Object::Dictionary(d) => d,
        _ => return Some(loc),
    };
    let uses_bare_type3 = fonts
        .iter()
        .filter_map(|(_, v)| v.as_reference().ok())
        .any(|id| type3_without_resources.contains(&id));
    (!uses_bare_type3).then_some(loc)
}

fn type3_fonts_without_resources(doc: &Document) -> HashSet<ObjectId> {
    doc.objects
        .iter()
        .filter(|(_, o)| {
            o.as_dict()
                .is_ok_and(|d| is_type3(d) && !d.has(b"Resources"))
        })
        .map(|(&id, _)| id)
        .collect()
}

fn is_type3(d: &Dictionary) -> bool {
    d.get(b"Subtype")
        .and_then(Object::as_name)
        .is_ok_and(|s| s == b"Type3")
}

fn is_form_or_pattern(d: &Dictionary) -> bool {
    d.get(b"Subtype")
        .and_then(Object::as_name)
        .is_ok_and(|s| s == b"Form")
        || d.get(b"PatternType")
            .and_then(Object::as_i64)
            .is_ok_and(|p| p == 1)
}

fn charprocs_content(doc: &Document, font: &Dictionary) -> Vec<u8> {
    let mut out = Vec::new();
    let Ok(procs) = font.get(b"CharProcs").and_then(|p| p.as_dict()) else {
        return out;
    };
    for (_, value) in procs.iter() {
        if let Ok(id) = value.as_reference()
            && let Ok(Object::Stream(s)) = doc.get_object(id)
            && let Ok(bytes) = s.decompressed_content_with_limit(MAX_CONTENT_BYTES)
        {
            out.extend(bytes);
            out.push(b'\n');
        }
    }
    out
}

fn res_loc(owner: &Dictionary, owner_id: ObjectId) -> Option<ResLoc> {
    match owner.get(b"Resources").ok()? {
        Object::Reference(id) => Some(ResLoc::Indirect(*id)),
        Object::Dictionary(_) => Some(ResLoc::InlineIn(owner_id)),
        _ => None,
    }
}

fn resources_dict(doc: &Document, loc: ResLoc) -> Option<&Dictionary> {
    match loc {
        ResLoc::Indirect(id) => doc.get_dictionary(id).ok(),
        ResLoc::InlineIn(owner) => owner_dict(doc, owner)?
            .get(b"Resources")
            .ok()?
            .as_dict()
            .ok(),
    }
}

fn owner_dict(doc: &Document, id: ObjectId) -> Option<&Dictionary> {
    match doc.get_object(id).ok()? {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

// ------------------------------------------------------------- analysis

/// Names used by a content stream, or `None` if it cannot be trusted.
fn parse_usage(content: &[u8]) -> Option<Usage> {
    let ops = Content::decode(content).ok()?;
    let mut usage = Usage::default();
    for op in &ops.operations {
        record(op, &mut usage);
    }
    Some(usage)
}

fn record(op: &Operation, usage: &mut Usage) {
    if matches!(op.operator.as_str(), "BI" | "ID" | "EI") {
        usage.keep_all[1] = true;
        return;
    }
    let Some((category, operand)) = resource_operand(op) else {
        return;
    };
    if let Some(Object::Name(name)) = op.operands.get(operand) {
        usage.names[category].insert(name.clone());
    }
}

/// For operators that name a resource: (category index, operand index).
fn resource_operand(op: &Operation) -> Option<(usize, usize)> {
    Some(match op.operator.as_str() {
        "Do" => (4, 0),
        "Tf" => (5, 0),
        "gs" => (0, 0),
        "cs" | "CS" => (1, 0),
        "scn" | "SCN" => (2, op.operands.len().checked_sub(1)?),
        "sh" => (3, 0),
        "BDC" | "DP" => (6, 1),
        _ => return None,
    })
}

fn merge_usage(into: &mut Usage, from: Usage) {
    for i in 0..CATEGORIES.len() {
        into.names[i].extend(from.names[i].iter().cloned());
        into.keep_all[i] |= from.keep_all[i];
    }
}

// ----------------------------------------------------------------- plan

type Plan = Vec<(CatLoc, Vec<Vec<u8>>)>;

/// Category dictionaries can themselves be shared indirect objects, so
/// usage is unioned per category location before deciding what to drop.
fn plan_removals(
    doc: &Document,
    usage: &HashMap<ResLoc, Usage>,
    skipped: &HashSet<ResLoc>,
) -> Plan {
    let mut used: HashMap<CatLoc, HashSet<Vec<u8>>> = HashMap::new();
    let mut keep_whole: HashSet<CatLoc> = HashSet::new();
    for (&loc, u) in usage {
        for (i, cat_loc) in category_locations(doc, loc) {
            if skipped.contains(&loc) || u.keep_all[i] {
                keep_whole.insert(cat_loc);
            }
            used.entry(cat_loc)
                .or_default()
                .extend(u.names[i].iter().cloned());
        }
    }
    for &loc in skipped {
        for (_, cat_loc) in category_locations(doc, loc) {
            keep_whole.insert(cat_loc);
        }
    }
    used.into_iter()
        .filter(|(cat_loc, _)| !keep_whole.contains(cat_loc))
        .filter_map(|(cat_loc, names)| {
            let dict = category_dict(doc, cat_loc)?;
            let remove: Vec<Vec<u8>> = dict
                .iter()
                .map(|(k, _)| k.clone())
                .filter(|k| !names.contains(k))
                .collect();
            (!remove.is_empty()).then_some((cat_loc, remove))
        })
        .collect()
}

fn category_locations(doc: &Document, loc: ResLoc) -> Vec<(usize, CatLoc)> {
    let Some(res) = resources_dict(doc, loc) else {
        return Vec::new();
    };
    CATEGORIES
        .iter()
        .enumerate()
        .filter_map(|(i, cat)| match res.get(cat).ok()? {
            Object::Reference(id) => Some((i, CatLoc::Indirect(*id))),
            Object::Dictionary(_) => Some((i, CatLoc::InlineIn(loc, i))),
            _ => None,
        })
        .collect()
}

fn category_dict(doc: &Document, cat_loc: CatLoc) -> Option<&Dictionary> {
    match cat_loc {
        CatLoc::Indirect(id) => doc.get_dictionary(id).ok(),
        CatLoc::InlineIn(loc, i) => resources_dict(doc, loc)?
            .get(CATEGORIES[i])
            .ok()?
            .as_dict()
            .ok(),
    }
}

fn apply_removals(doc: &mut Document, plan: Plan) -> usize {
    let mut removed = 0;
    for (cat_loc, names) in plan {
        let Some(dict) = category_dict_mut(doc, cat_loc) else {
            continue;
        };
        for name in names {
            removed += dict.remove(&name).is_some() as usize;
        }
    }
    removed
}

fn category_dict_mut(doc: &mut Document, cat_loc: CatLoc) -> Option<&mut Dictionary> {
    match cat_loc {
        CatLoc::Indirect(id) => doc.get_dictionary_mut(id).ok(),
        CatLoc::InlineIn(loc, i) => resources_dict_mut(doc, loc)?
            .get_mut(CATEGORIES[i])
            .ok()?
            .as_dict_mut()
            .ok(),
    }
}

fn resources_dict_mut(doc: &mut Document, loc: ResLoc) -> Option<&mut Dictionary> {
    match loc {
        ResLoc::Indirect(id) => doc.get_dictionary_mut(id).ok(),
        ResLoc::InlineIn(owner) => {
            let dict = match doc.get_object_mut(owner).ok()? {
                Object::Dictionary(d) => d,
                Object::Stream(s) => &mut s.dict,
                _ => return None,
            };
            dict.get_mut(b"Resources").ok()?.as_dict_mut().ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use lopdf::{Stream, dictionary};

    use super::*;

    /// One page whose content is `content`, with inline resources holding
    /// fonts F1 and F2 and XObjects Im1 and Im2. Returns (doc, page id).
    fn page_doc(content: &[u8]) -> (Document, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let font = |doc: &mut Document| {
            doc.add_object(
                dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
            )
        };
        let f1 = font(&mut doc);
        let f2 = font(&mut doc);
        let image = |doc: &mut Document| {
            doc.add_object(Stream::new(
                dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 1, "Height" => 1,
                              "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
                vec![0],
            ))
        };
        let im1 = image(&mut doc);
        let im2 = image(&mut doc);
        let contents = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => contents,
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => f1, "F2" => f2 },
                "XObject" => dictionary! { "Im1" => im1, "Im2" => im2 },
            },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        (doc, page)
    }

    fn resource_names(doc: &Document, page: ObjectId, category: &[u8]) -> Vec<String> {
        let page = doc.get_dictionary(page).unwrap();
        let res = page.get(b"Resources").unwrap().as_dict().unwrap();
        let mut names: Vec<String> = res
            .get(category)
            .unwrap()
            .as_dict()
            .unwrap()
            .iter()
            .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn unused_entries_are_removed() {
        let (mut doc, page) = page_doc(b"BT /F1 12 Tf (x) Tj ET q /Im2 Do Q");
        assert_eq!(prune_unused(&mut doc), 2);
        assert_eq!(resource_names(&doc, page, b"Font"), ["F1"]);
        assert_eq!(resource_names(&doc, page, b"XObject"), ["Im2"]);
    }

    #[test]
    fn nothing_used_removes_everything_in_touched_categories() {
        let (mut doc, page) = page_doc(b"0 0 m 1 1 l S");
        assert_eq!(prune_unused(&mut doc), 4);
        assert!(resource_names(&doc, page, b"Font").is_empty());
    }

    #[test]
    fn inline_image_keeps_color_spaces_whole() {
        let (mut doc, page) = page_doc(b"BI /W 1 /H 1 /CS /CS0 /BPC 8 ID \x00 EI /F1 1 Tf");
        let res = doc
            .get_dictionary_mut(page)
            .unwrap()
            .get_mut(b"Resources")
            .unwrap()
            .as_dict_mut()
            .unwrap();
        res.set(
            "ColorSpace",
            dictionary! { "CS0" => "DeviceGray", "CS1" => "DeviceRGB" },
        );
        prune_unused(&mut doc);
        assert_eq!(resource_names(&doc, page, b"ColorSpace"), ["CS0", "CS1"]);
    }

    #[test]
    fn inherited_resources_are_left_alone() {
        let (mut doc, page) = page_doc(b"/F1 1 Tf");
        // Move the resources up to the Pages node, as an indirect object.
        let res = doc
            .get_dictionary_mut(page)
            .unwrap()
            .remove(b"Resources")
            .unwrap();
        let res_id = doc.add_object(res);
        let pages_id = doc
            .get_dictionary(page)
            .unwrap()
            .get(b"Parent")
            .unwrap()
            .as_reference()
            .unwrap();
        doc.get_dictionary_mut(pages_id)
            .unwrap()
            .set("Resources", res_id);
        assert_eq!(prune_unused(&mut doc), 0);
    }
}
