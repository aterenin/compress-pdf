//! CMaps for Type 0 fonts: splitting a string into character codes and
//! mapping each to a CID. The two Identity CMaps are built in; embedded
//! CMap streams are parsed (`codespacerange`, `cidrange`, `cidchar`,
//! `usecmap` of Identity). Other predefined CMaps need tables this crate
//! does not carry, so they are reported as unsupported and the font is
//! left alone.

use lopdf::Object;
use lopdf::content::Content;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Codespace {
    bytes: usize,
    low: u32,
    high: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Range {
    bytes: usize,
    low: u32,
    high: u32,
    cid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CMap {
    codespaces: Vec<Codespace>,
    ranges: Vec<Range>,
    identity: bool,
}

impl CMap {
    /// Identity-H / Identity-V: two-byte codes, CID = code.
    pub fn identity() -> CMap {
        CMap {
            codespaces: vec![Codespace {
                bytes: 2,
                low: 0,
                high: 0xFFFF,
            }],
            ranges: Vec::new(),
            identity: true,
        }
    }

    /// A predefined CMap by name; only the Identity ones are known.
    pub fn predefined(name: &[u8]) -> Option<CMap> {
        matches!(name, b"Identity-H" | b"Identity-V").then(CMap::identity)
    }

    /// Parse an embedded CMap stream's content.
    pub fn parse(content: &[u8]) -> Option<CMap> {
        let ops = Content::decode_strict(content).ok()?;
        let mut cmap = CMap {
            codespaces: Vec::new(),
            ranges: Vec::new(),
            identity: false,
        };
        for op in &ops.operations {
            match op.operator.as_str() {
                "endcodespacerange" => {
                    for pair in op.operands.as_chunks::<2>().0 {
                        let (low, bytes) = code_bytes(&pair[0])?;
                        let (high, _) = code_bytes(&pair[1])?;
                        cmap.codespaces.push(Codespace { bytes, low, high });
                    }
                }
                // `bfrange` and `bfchar` belong in ToUnicode CMaps, but
                // producers use them in encoding CMaps too, with the
                // destination string read as a big-endian CID.
                "endcidrange" | "endbfrange" => {
                    for triple in op.operands.as_chunks::<3>().0 {
                        let (low, bytes) = code_bytes(&triple[0])?;
                        let (high, _) = code_bytes(&triple[1])?;
                        let cid = cid_operand(&triple[2])?;
                        cmap.ranges.push(Range {
                            bytes,
                            low,
                            high,
                            cid,
                        });
                    }
                }
                "endcidchar" | "endbfchar" => {
                    for pair in op.operands.as_chunks::<2>().0 {
                        let (code, bytes) = code_bytes(&pair[0])?;
                        let cid = cid_operand(&pair[1])?;
                        cmap.ranges.push(Range {
                            bytes,
                            low: code,
                            high: code,
                            cid,
                        });
                    }
                }
                "usecmap" => {
                    let parent = op.operands.first()?.as_name().ok()?;
                    let parent = CMap::predefined(parent)?;
                    cmap.codespaces.extend(parent.codespaces);
                    cmap.identity = parent.identity;
                }
                _ => {}
            }
        }
        if cmap.codespaces.is_empty() {
            // Codespace ranges are required; without them fall back to the
            // byte lengths the mappings themselves use.
            let mut lengths: Vec<usize> = cmap.ranges.iter().map(|r| r.bytes).collect();
            lengths.sort_unstable();
            lengths.dedup();
            if lengths.is_empty() {
                return None;
            }
            for bytes in lengths {
                cmap.codespaces.push(Codespace {
                    bytes,
                    low: 0,
                    high: u32::MAX >> (32 - 8 * bytes.min(4)),
                });
            }
        }
        Some(cmap)
    }

    /// The CIDs a string selects. Codes that fall in no codespace consume
    /// one byte (as readers do) and map to CID 0.
    pub fn cids(&self, string: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < string.len() {
            let (code, bytes) = self.next_code(&string[i..]);
            out.push(self.cid(code, bytes));
            i += bytes;
        }
        out
    }

    fn next_code(&self, s: &[u8]) -> (u32, usize) {
        // Try each byte length in increasing order; the first codespace
        // that contains the prefix wins.
        for bytes in 1..=4.min(s.len()) {
            let code = s[..bytes]
                .iter()
                .fold(0u32, |acc, b| (acc << 8) | u32::from(*b));
            if self
                .codespaces
                .iter()
                .any(|c| c.bytes == bytes && (c.low..=c.high).contains(&code))
            {
                return (code, bytes);
            }
        }
        // No match: the shortest codespace length, or one byte.
        let bytes = self
            .codespaces
            .iter()
            .map(|c| c.bytes)
            .min()
            .unwrap_or(1)
            .min(s.len());
        let code = s[..bytes]
            .iter()
            .fold(0u32, |acc, b| (acc << 8) | u32::from(*b));
        (code, bytes)
    }

    fn cid(&self, code: u32, bytes: usize) -> u32 {
        if let Some(r) = self
            .ranges
            .iter()
            .find(|r| r.bytes == bytes && (r.low..=r.high).contains(&code))
        {
            return r.cid + (code - r.low);
        }
        if self.identity { code } else { 0 }
    }
}

/// A code given as a PDF string: its value and byte length.
/// A CID operand: an integer, or a string holding one big-endian.
fn cid_operand(obj: &Object) -> Option<u32> {
    match obj {
        Object::Integer(i) => u32::try_from(*i).ok(),
        Object::String(..) => code_bytes(obj).map(|(cid, _)| cid),
        _ => None,
    }
}

fn code_bytes(obj: &Object) -> Option<(u32, usize)> {
    let Object::String(bytes, _) = obj else {
        return None;
    };
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    let code = bytes.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b));
    Some((code, bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_splits_two_byte_codes() {
        let cmap = CMap::predefined(b"Identity-H").unwrap();
        assert_eq!(cmap.cids(&[0, 65, 1, 2]), vec![65, 258]);
        assert!(CMap::predefined(b"UniGB-UCS2-H").is_none());
    }

    #[test]
    fn embedded_cmap_maps_ranges_and_singles() {
        let src = b"/CIDInit /ProcSet findresource begin begincmap
            1 begincodespacerange <00> <80> <8140> <9ffc> endcodespacerange
            2 begincidrange <20> <7e> 1 <8140> <8150> 633 endcidrange
            1 begincidchar <7f> 999 endcidchar
            endcmap";
        let cmap = CMap::parse(src).unwrap();
        // 'A' (0x41) -> 1 + 0x21 = 34; 0x8142 -> 635; 0x7f -> 999; 0x81 alone
        // is in no codespace and maps to 0 as a one-byte code.
        assert_eq!(cmap.cids(&[0x41, 0x81, 0x42, 0x7f]), vec![34, 635, 999]);
        assert_eq!(cmap.cids(&[0x00]), vec![0]);
    }

    #[test]
    fn bf_operators_map_to_cids_read_from_strings() {
        let src = b"1 begincodespacerange <0000> <ffff> endcodespacerange
            1 beginbfchar <0020> <0003> endbfchar
            1 beginbfrange <0041> <005a> <0024> endbfrange";
        let cmap = CMap::parse(src).unwrap();
        assert_eq!(cmap.cids(&[0, 0x20, 0, 0x43]), vec![3, 0x26]);
    }

    #[test]
    fn usecmap_identity_and_missing_codespaces() {
        let src = b"/Identity-H usecmap 1 begincidrange <0100> <01ff> 5 endcidrange";
        let cmap = CMap::parse(src).unwrap();
        assert_eq!(cmap.cids(&[1, 0, 0, 7]), vec![5, 7]);
        let no_space = b"1 begincidchar <41> 3 endcidchar";
        assert_eq!(CMap::parse(no_space).unwrap().cids(b"AB"), vec![3, 0]);
        assert!(CMap::parse(b"nothing here").is_none());
    }
}
