//! Content-stream walker: tracks the CTM and the current clip through a
//! page's content, descends into form XObjects, tiling patterns and
//! annotation appearance streams, and records every image placement.
//!
//! Clip tracking is by bounding box: a path used with `W`/`W*` clips to the
//! bounding box of its points in device space. That is exact for
//! rectangles and conservative (never too small) for everything else,
//! which is what the image stage needs to crop safely.

use std::collections::HashSet;

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId};

use super::geometry::{Matrix, Rect};
use super::{ImageUsage, Placement};

/// Nested forms deeper than this are not followed (cycle and bomb guard).
const MAX_DEPTH: usize = 16;
const MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
struct State {
    ctm: Matrix,
    clip: Rect,
}

/// Resource dictionaries to search, innermost first.
type Chain = Vec<Dictionary>;

pub struct Walker<'a> {
    doc: &'a Document,
    usage: &'a mut ImageUsage,
    depth: usize,
    in_progress: HashSet<ObjectId>,
}

impl<'a> Walker<'a> {
    pub fn new(doc: &'a Document, usage: &'a mut ImageUsage) -> Self {
        Walker {
            doc,
            usage,
            depth: 0,
            in_progress: HashSet::new(),
        }
    }

    /// Walk one page: its content under the identity CTM clipped to the
    /// crop box, then its annotation appearance streams.
    pub fn walk_page(&mut self, page_id: ObjectId) {
        let Some(chain) = page_resource_chain(self.doc, page_id) else {
            return;
        };
        let clip = page_clip(self.doc, page_id);
        if let Some(content) = page_content(self.doc, page_id) {
            self.walk(
                &content,
                &chain,
                State {
                    ctm: Matrix::IDENTITY,
                    clip,
                },
            );
        }
        self.walk_annotations(page_id, &chain);
    }

    fn walk(&mut self, content: &[u8], chain: &Chain, initial: State) {
        let Ok(ops) = Content::decode(content) else {
            return;
        };
        let mut state = initial;
        let mut stack: Vec<State> = Vec::new();
        let mut path = PathTracker::default();
        for op in &ops.operations {
            match op.operator.as_str() {
                "q" => stack.push(state),
                "Q" => state = stack.pop().unwrap_or(initial),
                "cm" => {
                    if let Some(m) = matrix_operands(&op.operands) {
                        state.ctm = m.then(state.ctm);
                    }
                }
                "Do" => self.do_xobject(op, chain, state),
                "scn" | "SCN" => self.paint_pattern(op, chain, state),
                _ => path.observe(op, &mut state),
            }
        }
    }

    fn do_xobject(&mut self, op: &Operation, chain: &Chain, state: State) {
        let Some(Object::Name(name)) = op.operands.first() else {
            return;
        };
        let Some((id, stream)) = lookup_stream(self.doc, chain, b"XObject", name) else {
            return;
        };
        match stream.dict.get(b"Subtype").and_then(Object::as_name) {
            Ok(b"Image") => self.place_image(id, &stream.dict, state),
            Ok(b"Form") => self.walk_form(id, &stream, chain, state),
            _ => {}
        }
    }

    fn place_image(&mut self, id: ObjectId, dict: &Dictionary, state: State) {
        let width = dict
            .get(b"Width")
            .and_then(Object::as_i64)
            .unwrap_or(0)
            .max(0) as u32;
        let height = dict
            .get(b"Height")
            .and_then(Object::as_i64)
            .unwrap_or(0)
            .max(0) as u32;
        let (width_pt, height_pt) = state.ctm.unit_extent();
        let crop = visible_fraction(state.ctm, state.clip);
        let usage = self.usage.by_object.entry(id).or_default();
        usage.pixels = (width, height);
        usage.placements.push(Placement {
            width_pt,
            height_pt,
            crop,
        });
    }

    fn walk_form(&mut self, id: ObjectId, form: &lopdf::Stream, chain: &Chain, state: State) {
        if self.depth >= MAX_DEPTH || self.in_progress.contains(&id) {
            return;
        }
        let Ok(content) = form.decompressed_content_with_limit(MAX_CONTENT_BYTES) else {
            return;
        };
        let matrix = form
            .dict
            .get(b"Matrix")
            .ok()
            .and_then(|m| matrix_operands(m.as_array().ok()?))
            .unwrap_or(Matrix::IDENTITY);
        let ctm = matrix.then(state.ctm);
        let clip = match rect_from(self.doc, form.dict.get(b"BBox").ok()) {
            Some(bbox) => state.clip.intersect(bbox.transformed(ctm)),
            None => state.clip,
        };
        let mut inner = chain.clone();
        if let Some(res) = own_resources(self.doc, &form.dict) {
            inner.insert(0, res);
        }
        self.depth += 1;
        self.in_progress.insert(id);
        self.walk(&content, &inner, State { ctm, clip });
        self.in_progress.remove(&id);
        self.depth -= 1;
    }

    /// A tiling pattern's content is drawn in pattern space: the pattern
    /// matrix relative to the default space of the page (not the CTM at the
    /// time of painting). The current clip still applies.
    fn paint_pattern(&mut self, op: &Operation, chain: &Chain, state: State) {
        let Some(Object::Name(name)) = op.operands.last() else {
            return;
        };
        let Some((id, stream)) = lookup_stream(self.doc, chain, b"Pattern", name) else {
            return;
        };
        if stream
            .dict
            .get(b"PatternType")
            .and_then(Object::as_i64)
            .ok()
            != Some(1)
        {
            return;
        }
        let base = State {
            ctm: Matrix::IDENTITY,
            clip: state.clip,
        };
        self.walk_form(id, &stream, chain, base);
    }

    fn walk_annotations(&mut self, page_id: ObjectId, chain: &Chain) {
        let Ok(annots) = self.doc.get_page_annotations(page_id) else {
            return;
        };
        let targets: Vec<(ObjectId, Rect)> = annots
            .iter()
            .filter_map(|a| {
                Some((
                    appearance_stream(self.doc, a)?,
                    rect_from(self.doc, a.get(b"Rect").ok())?,
                ))
            })
            .collect();
        for (id, rect) in targets {
            let Ok(Object::Stream(form)) = self.doc.get_object(id) else {
                continue;
            };
            let ctm = appearance_ctm(self.doc, &form.dict, rect);
            self.walk_form(id, form, chain, State { ctm, clip: rect });
        }
    }
}

// ------------------------------------------------------------- geometry

/// The fraction of an image's unit square that can be visible under `clip`,
/// or `None` when the whole image is visible.
fn visible_fraction(ctm: Matrix, clip: Rect) -> Option<Rect> {
    let inv = ctm.invert()?;
    let visible = clip.intersect(ctm.unit_square_bbox());
    if visible.is_empty() {
        return Some(Rect::new(0.0, 0.0, 0.0, 0.0));
    }
    let crop = visible.transformed(inv).intersect(Rect::UNIT);
    (!crop.covers(Rect::UNIT)).then_some(crop)
}

/// Where an annotation's appearance form lands: the form's BBox under its
/// Matrix is fitted to the annotation's Rect (PDF 32000-1, 12.5.5).
fn appearance_ctm(doc: &Document, form: &Dictionary, rect: Rect) -> Matrix {
    let matrix = form
        .get(b"Matrix")
        .ok()
        .and_then(|m| matrix_operands(m.as_array().ok()?))
        .unwrap_or(Matrix::IDENTITY);
    let Some(bbox) = rect_from(doc, form.get(b"BBox").ok()) else {
        return Matrix::IDENTITY;
    };
    let tb = bbox.transformed(matrix);
    let sx = if tb.width() > 0.0 {
        rect.width() / tb.width()
    } else {
        1.0
    };
    let sy = if tb.height() > 0.0 {
        rect.height() / tb.height()
    } else {
        1.0
    };
    Matrix::from_array([sx, 0.0, 0.0, sy, rect.x0 - tb.x0 * sx, rect.y0 - tb.y0 * sy])
}

/// Tracks the current path's points in device space and applies pending
/// clips when a painting operator ends the path.
#[derive(Default)]
struct PathTracker {
    points: Vec<(f32, f32)>,
    start: (f32, f32),
    pending_clip: bool,
}

impl PathTracker {
    fn observe(&mut self, op: &Operation, state: &mut State) {
        let nums: Vec<f32> = op.operands.iter().filter_map(number).collect();
        match op.operator.as_str() {
            "m" | "l" if nums.len() >= 2 => self.point(state.ctm, nums[0], nums[1]),
            "c" if nums.len() >= 6 => self.points(state.ctm, &nums),
            "v" | "y" if nums.len() >= 4 => self.points(state.ctm, &nums),
            "re" if nums.len() >= 4 => {
                let (x, y, w, h) = (nums[0], nums[1], nums[2], nums[3]);
                self.points(state.ctm, &[x, y, x + w, y, x + w, y + h, x, y + h]);
            }
            "W" | "W*" => self.pending_clip = true,
            "n" | "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" => self.end(state),
            _ => {}
        }
    }

    fn point(&mut self, ctm: Matrix, x: f32, y: f32) {
        let p = ctm.apply(x, y);
        if self.points.is_empty() {
            self.start = p;
        }
        self.points.push(p);
    }

    fn points(&mut self, ctm: Matrix, coords: &[f32]) {
        for pair in coords.as_chunks::<2>().0 {
            self.point(ctm, pair[0], pair[1]);
        }
    }

    fn end(&mut self, state: &mut State) {
        if self.pending_clip {
            let bbox = Rect::from_points(&self.points).unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0));
            state.clip = state.clip.intersect(bbox);
        }
        self.pending_clip = false;
        self.points.clear();
    }
}

// ------------------------------------------------------------ resources

fn page_resource_chain(doc: &Document, page_id: ObjectId) -> Option<Chain> {
    let (inline, ids) = doc.get_page_resources(page_id).ok()?;
    let mut chain: Chain = inline.cloned().into_iter().collect();
    chain.extend(
        ids.iter()
            .filter_map(|id| doc.get_dictionary(*id).ok().cloned()),
    );
    Some(chain)
}

fn own_resources(doc: &Document, dict: &Dictionary) -> Option<Dictionary> {
    match dict.get(b"Resources").ok()? {
        Object::Dictionary(d) => Some(d.clone()),
        Object::Reference(id) => doc.get_dictionary(*id).ok().cloned(),
        _ => None,
    }
}

/// Resolve a named resource of `category` to an indirect stream.
fn lookup_stream(
    doc: &Document,
    chain: &Chain,
    category: &[u8],
    name: &[u8],
) -> Option<(ObjectId, lopdf::Stream)> {
    for res in chain {
        let Some(cat) = res.get(category).ok().and_then(|c| deref(doc, c)) else {
            continue;
        };
        let Ok(Object::Reference(id)) = cat.as_dict().and_then(|d| d.get(name)) else {
            continue;
        };
        let id: ObjectId = id.to_owned();
        if let Ok(Object::Stream(s)) = doc.get_object(id) {
            return Some((id, s.clone()));
        }
    }
    None
}

fn appearance_stream(doc: &Document, annot: &Dictionary) -> Option<ObjectId> {
    let ap = deref(doc, annot.get(b"AP").ok()?)?.as_dict().ok()?;
    match ap.get(b"N").ok()? {
        Object::Reference(id) => Some(id.to_owned()),
        Object::Dictionary(states) => {
            let key = annot.get(b"AS").and_then(Object::as_name).ok();
            let entry = match key {
                Some(k) => states.get(k).ok(),
                None => states.iter().next().map(|(_, v)| v),
            };
            entry?.as_reference().ok()
        }
        _ => None,
    }
}

fn page_content(doc: &Document, page_id: ObjectId) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for id in doc.get_page_contents(page_id) {
        let Ok(Object::Stream(s)) = doc.get_object(id) else {
            return None;
        };
        out.extend(s.decompressed_content_with_limit(MAX_CONTENT_BYTES).ok()?);
        out.push(b'\n');
    }
    Some(out)
}

fn page_clip(doc: &Document, page_id: ObjectId) -> Rect {
    let page = doc.get_dictionary(page_id).ok();
    let media = page.and_then(|p| rect_from(doc, p.get(b"MediaBox").ok()));
    let crop = page.and_then(|p| rect_from(doc, p.get(b"CropBox").ok()));
    match (media, crop) {
        (Some(m), Some(c)) => m.intersect(c),
        (Some(m), None) => m,
        (None, Some(c)) => c,
        (None, None) => Rect::EVERYTHING,
    }
}

// ------------------------------------------------------------- operands

/// Follow a reference, if it is one.
fn deref<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    doc.dereference(obj).ok().map(|(_, o)| o)
}

fn number(obj: &Object) -> Option<f32> {
    match obj {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}

fn matrix_operands(operands: &[Object]) -> Option<Matrix> {
    let n: Vec<f32> = operands.iter().filter_map(number).collect();
    (n.len() == 6).then(|| Matrix::from_array([n[0], n[1], n[2], n[3], n[4], n[5]]))
}

fn rect_from(doc: &Document, obj: Option<&Object>) -> Option<Rect> {
    let arr = deref(doc, obj?)?.as_array().ok()?;
    let n: Vec<f32> = arr.iter().filter_map(|o| number(deref(doc, o)?)).collect();
    (n.len() == 4).then(|| Rect::from_corners(n[0], n[1], n[2], n[3]))
}
