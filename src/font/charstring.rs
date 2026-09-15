//! Type 1 charstrings translated to Type 2 (CFF) charstrings.
//!
//! The two languages share their path operators; what differs is what a
//! translation has to reconcile. The choices below follow what pdf.js
//! settled on for its own Type 1 to CFF conversion (studied, not
//! copied; this is an independent implementation):
//!
//! - `hsbw`/`sbw` become the advance width (which Type 2 carries as an
//!   extra first argument) plus a move to the sidebearing point, which is
//!   where Type 1 leaves the current point.
//! - Subroutines are expanded in place: Type 1 uses them for hint
//!   replacement and flex, neither of which survives the translation.
//! - Flex (OtherSubrs 0 to 2) turns into the Type 2 `flex` operator; the
//!   reference point is folded into the first control point.
//! - `seac` becomes the four-argument `endchar`, with the accent offset
//!   corrected by the composite's own sidebearing minus the accent's.
//! - `div` is evaluated; a fractional result is written as a 16.16
//!   fixed-point number.
//! - Hints (`hstem`, `vstem`, the `stem3` forms, `dotsection`, hint
//!   replacement) are dropped: Type 2 wants every hint before the first
//!   path operator and expresses replacement through masks, and viewers
//!   render unhinted PDF fonts anyway.
//! - `closepath` and `setcurrentpoint` have no Type 2 counterpart and are
//!   dropped, as are the reserved codes a few generators emit.
//!
//! Anything unexpected makes the whole translation fail, so the caller
//! keeps the Type 1 program rather than ship a glyph that lost its shape.

const MAX_DEPTH: usize = 30;

/// A translated glyph.
#[derive(Debug, Clone, PartialEq)]
pub struct Glyph {
    /// Type 2 charstring bytes, width included.
    pub charstring: Vec<u8>,
    pub width: f64,
}

pub fn to_type2(type1: &[u8], subrs: &[Vec<u8>]) -> Option<Glyph> {
    let mut t = Translator {
        subrs,
        stack: Vec::new(),
        ps_stack: Vec::new(),
        out: Vec::new(),
        flex: None,
        width: 0.0,
        sbx: 0.0,
    };
    t.run(type1, 0)?;
    Some(Glyph {
        charstring: t.out,
        width: t.width,
    })
}

struct Translator<'a> {
    subrs: &'a [Vec<u8>],
    stack: Vec<f64>,
    /// Values OtherSubrs leave for `pop` to fetch.
    ps_stack: Vec<f64>,
    out: Vec<u8>,
    /// Coordinates collected while a flex is in progress.
    flex: Option<Vec<f64>>,
    width: f64,
    sbx: f64,
}

impl Translator<'_> {
    /// Interpret one charstring (the glyph's or a subroutine's). Returns
    /// `Some(true)` when the glyph ended.
    fn run(&mut self, code: &[u8], depth: usize) -> Option<bool> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut i = 0;
        while i < code.len() {
            let b = code[i];
            i += 1;
            if b >= 32 {
                let (value, used) = number(&code[i - 1..])?;
                i += used - 1;
                self.stack.push(value);
                continue;
            }
            let op = if b == 12 {
                i += 1;
                256 + u16::from(*code.get(i - 1)?)
            } else {
                u16::from(b)
            };
            match self.step(op, depth)? {
                Flow::Continue => {}
                Flow::Return => return Some(false),
                Flow::End => return Some(true),
            }
        }
        Some(false)
    }

    fn step(&mut self, op: u16, depth: usize) -> Option<Flow> {
        match op {
            // hstem, vstem, dotsection, vstem3, hstem3, setcurrentpoint
            1 | 3 | 256 | 257 | 258 | 289 => self.stack.clear(),
            9 => self.stack.clear(), // closepath
            // Reserved single-byte codes, which some generators emit; the
            // renderers this follows skip them, and so does this.
            0 | 2 | 15..=20 | 23..=29 => self.stack.clear(),
            10 => return call_subr(self, depth),
            11 => return Some(Flow::Return),
            13 => hsbw(self)?,
            14 => {
                self.out.push(14);
                return Some(Flow::End);
            }
            _ => return self.step_more(op),
        }
        Some(Flow::Continue)
    }

    fn step_more(&mut self, op: u16) -> Option<Flow> {
        match op {
            4 | 21 | 22 => moveto(self, op)?,
            5 | 8 | 30 | 31 => self.emit(path_arity(op), &[op as u8])?,
            6 | 7 => self.emit(1, &[op as u8])?,
            262 => return seac(self),
            263 => sbw(self)?,
            268 => div(self)?,
            272 => call_othersubr(self)?,
            273 => pop_result(self)?,
            _ => return None,
        }
        Some(Flow::Continue)
    }
}

fn call_subr(t: &mut Translator<'_>, depth: usize) -> Option<Flow> {
    let n = t.stack.pop()?;
    let subr = t.subrs.get(n as usize)?;
    Some(if t.run(subr, depth + 1)? {
        Flow::End
    } else {
        Flow::Continue
    })
}

/// `sbx wx hsbw`: the width goes first, then a move to (sbx, 0).
fn hsbw(t: &mut Translator<'_>) -> Option<()> {
    let [sbx, wx] = t.take()?;
    t.width = wx;
    t.sbx = sbx;
    t.stack.extend([wx, sbx]);
    t.emit(2, &[22])
}

/// `sbx sby wx wy sbw`: like `hsbw` with a two-dimensional sidebearing.
fn sbw(t: &mut Translator<'_>) -> Option<()> {
    let [sbx, sby, wx, _wy] = t.take()?;
    t.width = wx;
    t.sbx = sbx;
    t.stack.extend([wx, sbx, sby]);
    t.emit(3, &[21])
}

fn moveto(t: &mut Translator<'_>, op: u16) -> Option<()> {
    if let Some(points) = &mut t.flex {
        let (dx, dy) = match op {
            21 => {
                let [dx, dy] = take_from(&mut t.stack)?;
                (dx, dy)
            }
            22 => (take_from::<1>(&mut t.stack)?[0], 0.0),
            _ => (0.0, take_from::<1>(&mut t.stack)?[0]),
        };
        points.extend([dx, dy]);
        t.stack.clear();
        return Some(());
    }
    t.emit(path_arity(op), &[op as u8])
}

fn div(t: &mut Translator<'_>) -> Option<()> {
    let [a, b] = t.take()?;
    if b == 0.0 {
        return None;
    }
    t.stack.push(a / b);
    Some(())
}

/// `asb adx ady bchar achar seac`: Type 2's four-argument `endchar`
/// measures the accent from the composite's origin, so the accent's
/// sidebearing is replaced by the composite's.
fn seac(t: &mut Translator<'_>) -> Option<Flow> {
    let [asb, adx, ady, bchar, achar] = t.take()?;
    t.stack.extend([adx - asb + t.sbx, ady, bchar, achar]);
    t.emit(4, &[14])?;
    Some(Flow::End)
}

/// `arg1 ... argn n othersubr# callothersubr`.
fn call_othersubr(t: &mut Translator<'_>) -> Option<()> {
    let [n, which] = t.take()?;
    let args = take_n(&mut t.stack, n as usize)?;
    match which as i64 {
        0 => end_flex(t, &args),
        1 => {
            t.flex = Some(Vec::new());
            Some(())
        }
        2 => Some(()),
        // Hint replacement (3) and anything else: the arguments come
        // back through `pop`, top of the PostScript stack first.
        _ => {
            t.ps_stack = args;
            Some(())
        }
    }
}

fn pop_result(t: &mut Translator<'_>) -> Option<()> {
    let v = t.ps_stack.pop()?;
    t.stack.push(v);
    Some(())
}

/// OtherSubrs 0 closes a flex: seven points were collected by the
/// movetos in between (the reference point and six curve points),
/// and the three arguments are the flex height and the end point.
fn end_flex(t: &mut Translator<'_>, args: &[f64]) -> Option<()> {
    let points = t.flex.take()?;
    if points.len() != 14 || args.len() != 3 {
        return None;
    }
    let mut flex = Vec::with_capacity(13);
    flex.extend([points[0] + points[2], points[1] + points[3]]);
    flex.extend_from_slice(&points[4..14]);
    flex.push(args[0]);
    t.stack = flex;
    t.emit(13, &[12, 35])?;
    // `pop pop setcurrentpoint` follows and wants the end point.
    t.ps_stack = vec![args[2], args[1]];
    Some(())
}

impl Translator<'_> {
    fn take<const N: usize>(&mut self) -> Option<[f64; N]> {
        take_from(&mut self.stack)
    }

    /// Write the top `n` stack values as Type 2 operands followed by the
    /// operator, and clear the stack.
    fn emit(&mut self, n: usize, op: &[u8]) -> Option<()> {
        if self.stack.len() < n {
            return None;
        }
        let start = self.stack.len() - n;
        for v in &self.stack[start..] {
            encode(*v, &mut self.out)?;
        }
        self.out.extend_from_slice(op);
        self.stack.clear();
        Some(())
    }
}

enum Flow {
    Continue,
    Return,
    End,
}

fn path_arity(op: u16) -> usize {
    match op {
        4 | 6 | 7 | 22 => 1,
        5 | 21 => 2,
        30 | 31 => 4,
        _ => 6,
    }
}

fn take_from<const N: usize>(stack: &mut Vec<f64>) -> Option<[f64; N]> {
    take_n(stack, N)?.try_into().ok()
}

fn take_n(stack: &mut Vec<f64>, n: usize) -> Option<Vec<f64>> {
    if stack.len() < n {
        return None;
    }
    Some(stack.split_off(stack.len() - n))
}

/// A Type 1 charstring number: the same one- and two-byte forms as
/// Type 2, and a 32-bit integer after 255 (where Type 2 has fixed point).
fn number(code: &[u8]) -> Option<(f64, usize)> {
    let b = u32::from(code[0]);
    Some(match b {
        32..=246 => (f64::from(b as i32 - 139), 1),
        247..=250 => (
            f64::from(((b - 247) * 256 + u32::from(*code.get(1)?)) as i32 + 108),
            2,
        ),
        251..=254 => (
            f64::from(-(((b - 251) * 256 + u32::from(*code.get(1)?)) as i32) - 108),
            2,
        ),
        _ => {
            let bytes = code.get(1..5)?;
            (
                f64::from(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
                5,
            )
        }
    })
}

/// A Type 2 operand: the shortest integer form when the value is whole,
/// else 16.16 fixed point. Values outside the fixed-point range cannot
/// be expressed.
fn encode(v: f64, out: &mut Vec<u8>) -> Option<()> {
    if v.fract() == 0.0 && (-32768.0..=32767.0).contains(&v) {
        let i = v as i32;
        if (-107..=107).contains(&i) {
            out.push((i + 139) as u8);
        } else {
            out.push(28);
            out.extend((i as i16).to_be_bytes());
        }
        return Some(());
    }
    if !(-32768.0..32768.0).contains(&v) {
        return None;
    }
    out.push(255);
    out.extend(((v * 65536.0).round() as i32).to_be_bytes());
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::type1::tests::charstring;

    fn t2(source: &str) -> Vec<u8> {
        // The same readable form for Type 2, whose operands and shared
        // operators encode identically for the values used here.
        let mut out = Vec::new();
        for word in source.split_whitespace() {
            match word {
                "rmoveto" => out.push(21),
                "hmoveto" => out.push(22),
                "vmoveto" => out.push(4),
                "rlineto" => out.push(5),
                "hlineto" => out.push(6),
                "vlineto" => out.push(7),
                "rrcurveto" => out.push(8),
                "endchar" => out.push(14),
                "flex" => out.extend([12, 35]),
                n => encode(n.parse().unwrap(), &mut out).unwrap(),
            }
        }
        out
    }

    #[test]
    fn width_sidebearing_and_path_translate() {
        let g = to_type2(
            &charstring(
                "50 600 hsbw 0 0 rmoveto 400 hlineto 400 vlineto -400 hlineto closepath endchar",
            ),
            &[],
        )
        .unwrap();
        assert_eq!(g.width, 600.0);
        assert_eq!(
            g.charstring,
            t2("600 50 hmoveto 0 0 rmoveto 400 hlineto 400 vlineto -400 hlineto endchar")
        );
    }

    #[test]
    fn subroutines_are_inlined_and_hints_dropped() {
        let subrs = vec![charstring("10 20 rlineto return")];
        let g = to_type2(
            &charstring("0 500 hsbw 1 2 hstem 3 4 vstem 5 5 rmoveto 0 callsubr endchar"),
            &subrs,
        )
        .unwrap();
        assert_eq!(
            g.charstring,
            t2("500 0 hmoveto 5 5 rmoveto 10 20 rlineto endchar")
        );
    }

    #[test]
    fn flex_becomes_the_type2_operator() {
        // Reference point (1,2); six points; height 50; end (100, 0).
        let src = "0 500 hsbw 0 0 rmoveto 0 1 callothersubr \
                   1 2 rmoveto 0 2 callothersubr \
                   10 0 rmoveto 0 2 callothersubr \
                   10 5 rmoveto 0 2 callothersubr \
                   10 5 rmoveto 0 2 callothersubr \
                   10 -5 rmoveto 0 2 callothersubr \
                   10 -5 rmoveto 0 2 callothersubr \
                   10 0 rmoveto 0 2 callothersubr \
                   50 100 0 3 0 callothersubr pop pop setcurrentpoint endchar";
        let g = to_type2(&charstring(src), &[]).unwrap();
        assert_eq!(
            g.charstring,
            t2("500 0 hmoveto 0 0 rmoveto 11 2 10 5 10 5 10 -5 10 -5 10 0 50 flex endchar")
        );
    }

    #[test]
    fn seac_hint_replacement_and_div() {
        let subrs = vec![charstring("1 2 hstem return")];
        // Hint replacement: `subr# 1 3 callothersubr pop callsubr`.
        let g = to_type2(
            &charstring("30 600 hsbw 0 1 3 callothersubr pop callsubr 100 4 div 0 rmoveto 20 10 20 200 194 seac"),
            &subrs,
        )
        .unwrap();
        // 100/4 = 25; seac: adx - asb + sbx = 10 - 20 + 30 = 20.
        assert_eq!(
            g.charstring,
            t2("600 30 hmoveto 25 0 rmoveto 20 20 200 194 endchar")
        );
    }

    #[test]
    fn fractions_and_errors() {
        let g = to_type2(&charstring("0 500 hsbw 1 3 div 0 rmoveto endchar"), &[]).unwrap();
        let mut expected = t2("500 0 hmoveto");
        expected.push(255);
        expected.extend(((1.0f64 / 3.0 * 65536.0).round() as i32).to_be_bytes());
        expected.extend(t2("0 rmoveto endchar"));
        assert_eq!(g.charstring, expected);
        assert!(to_type2(&charstring("0 500 hsbw 5 callsubr endchar"), &[]).is_none());
        assert!(to_type2(&charstring("0 500 hsbw rlineto endchar"), &[]).is_none());
        assert!(to_type2(&[12, 99], &[]).is_none());
        // A reserved code (15) between operators is skipped.
        let g = to_type2(&[139, 247, 170, 13, 139, 139, 15, 14], &[]).unwrap();
        assert_eq!(g.charstring, t2("278 0 hmoveto endchar"));
    }
}
