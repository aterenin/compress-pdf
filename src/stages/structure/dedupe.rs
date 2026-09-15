//! Merge objects that are byte-for-byte identical in canonical form, and
//! repoint every reference to the survivor.
//!
//! Only objects whose identity carries no meaning are eligible: streams, and
//! dictionaries that are not pages, page-tree nodes, the catalog,
//! annotations, or members of a parent/child tree (`/Parent`, `/Kids`,
//! `/P`). Merging is repeated until a pass finds nothing, because repointing
//! references can make previously different dictionaries identical.

use std::collections::HashMap;

use lopdf::{Dictionary, Document, Object, ObjectId};

const MAX_PASSES: usize = 8;

/// Returns the number of objects removed.
pub fn merge_duplicates(doc: &mut Document) -> usize {
    let mut total = 0;
    for _ in 0..MAX_PASSES {
        let merged = merge_pass(doc);
        if merged == 0 {
            break;
        }
        total += merged;
    }
    total
}

fn merge_pass(doc: &mut Document) -> usize {
    let mut survivors: HashMap<Vec<u8>, ObjectId> = HashMap::new();
    let mut remap: HashMap<ObjectId, ObjectId> = HashMap::new();
    for (&id, obj) in &doc.objects {
        if !eligible(obj) {
            continue;
        }
        let key = canonical(obj);
        match survivors.get(&key) {
            Some(&keep) => {
                remap.insert(id, keep);
            }
            None => {
                survivors.insert(key, id);
            }
        }
    }
    if remap.is_empty() {
        return 0;
    }
    doc.traverse_objects(|obj| {
        if let Object::Reference(id) = obj
            && let Some(&keep) = remap.get(id)
        {
            *id = keep;
        }
    });
    for id in remap.keys() {
        doc.objects.remove(id);
    }
    remap.len()
}

fn eligible(obj: &Object) -> bool {
    match obj {
        Object::Stream(s) => !has_type(&s.dict, &[b"ObjStm", b"XRef"]),
        Object::Dictionary(d) => {
            !has_type(d, &[b"Page", b"Pages", b"Catalog", b"Annot"])
                && !d.has(b"Parent")
                && !d.has(b"Kids")
                && !d.has(b"P")
        }
        _ => false,
    }
}

fn has_type(dict: &Dictionary, types: &[&[u8]]) -> bool {
    dict.get(b"Type")
        .and_then(Object::as_name)
        .is_ok_and(|t| types.contains(&t))
}

/// Canonical byte form: dictionary keys sorted, floats by bit pattern,
/// stream content included. Two objects with equal canonical form are
/// interchangeable for every reader.
fn canonical(obj: &Object) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(obj, &mut out);
    out
}

fn write_canonical(obj: &Object, out: &mut Vec<u8>) {
    match obj {
        Object::Array(items) => {
            out.push(b'[');
            for item in items {
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Object::Dictionary(d) => write_dict(d, out),
        Object::Stream(s) => {
            write_dict(&s.dict, out);
            write_bytes(b'S', &s.content, out);
        }
        scalar => write_scalar(scalar, out),
    }
}

fn write_scalar(obj: &Object, out: &mut Vec<u8>) {
    match obj {
        Object::Boolean(b) => out.extend([b'b', *b as u8]),
        Object::Integer(i) => {
            out.push(b'i');
            out.extend(i.to_le_bytes());
        }
        Object::Real(r) => {
            out.push(b'r');
            out.extend(r.to_bits().to_le_bytes());
        }
        Object::Name(n) => write_bytes(b'/', n, out),
        Object::String(s, _) => write_bytes(b'(', s, out),
        Object::Reference((n, g)) => {
            out.push(b'R');
            out.extend(n.to_le_bytes());
            out.extend(g.to_le_bytes());
        }
        _ => out.push(b'n'),
    }
}

fn write_dict(dict: &Dictionary, out: &mut Vec<u8>) {
    let mut entries: Vec<(&Vec<u8>, &Object)> = dict.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    out.push(b'<');
    for (key, value) in entries {
        write_bytes(b'/', key, out);
        write_canonical(value, out);
    }
    out.push(b'>');
}

fn write_bytes(tag: u8, bytes: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    out.extend((bytes.len() as u32).to_le_bytes());
    out.extend(bytes);
}

#[cfg(test)]
mod tests {
    use lopdf::{Stream, dictionary};

    use super::*;

    #[test]
    fn identical_streams_are_merged_and_references_repointed() {
        let mut doc = Document::with_version("1.5");
        let a = doc.add_object(Stream::new(dictionary! {"Length" => 3}, b"abc".to_vec()));
        let b = doc.add_object(Stream::new(dictionary! {"Length" => 3}, b"abc".to_vec()));
        let holder = doc.add_object(dictionary! { "First" => a, "Second" => b });
        doc.trailer.set("Root", holder);

        assert_eq!(merge_duplicates(&mut doc), 1);
        let holder = doc.get_dictionary(holder).unwrap();
        let first = holder.get(b"First").unwrap().as_reference().unwrap();
        let second = holder.get(b"Second").unwrap().as_reference().unwrap();
        assert_eq!(first, second);
        assert!(doc.get_object(first).is_ok());
    }

    #[test]
    fn pages_and_tree_members_are_never_merged() {
        let mut doc = Document::with_version("1.5");
        let p1 = doc.add_object(dictionary! { "Type" => "Page" });
        let p2 = doc.add_object(dictionary! { "Type" => "Page" });
        let parent = doc.add_object(dictionary! { "Kids" => vec![p1.into(), p2.into()] });
        let f1 = doc.add_object(dictionary! { "T" => "x", "Parent" => parent });
        let f2 = doc.add_object(dictionary! { "T" => "x", "Parent" => parent });
        doc.trailer.set("Root", parent);
        doc.trailer.set("Fields", vec![f1.into(), f2.into()]);
        assert_eq!(merge_duplicates(&mut doc), 0);
    }

    #[test]
    fn second_pass_merges_dictionaries_that_became_identical() {
        let mut doc = Document::with_version("1.5");
        let s1 = doc.add_object(Stream::new(dictionary! {}, b"x".to_vec()));
        let s2 = doc.add_object(Stream::new(dictionary! {}, b"x".to_vec()));
        let d1 = doc.add_object(dictionary! { "Stream" => s1 });
        let d2 = doc.add_object(dictionary! { "Stream" => s2 });
        let root = doc.add_object(dictionary! { "A" => d1, "B" => d2 });
        doc.trailer.set("Root", root);
        assert_eq!(merge_duplicates(&mut doc), 2);
        assert_eq!(doc.objects.len(), 3);
    }

    #[test]
    fn different_content_is_kept_apart() {
        let mut doc = Document::with_version("1.5");
        let a = doc.add_object(Stream::new(dictionary! {}, b"abc".to_vec()));
        let b = doc.add_object(Stream::new(dictionary! {}, b"abd".to_vec()));
        let root = doc.add_object(dictionary! { "A" => a, "B" => b });
        doc.trailer.set("Root", root);
        assert_eq!(merge_duplicates(&mut doc), 0);
    }
}
