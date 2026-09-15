//! PDF functions (types 0, 2, 3 and 4), as used by Separation and DeviceN
//! tint transforms. A function object is parsed once into a [`Function`]
//! and then evaluated per input tuple. The second half of the file is the
//! type 4 PostScript calculator: numbers and booleans share one stack, and
//! integers are kept as whole `f64`s, which is exact for every value the
//! calculator can produce.

use std::fmt;

use lopdf::{Dictionary, Document, Object};

const MAX_SAMPLE_BYTES: usize = 64 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum Function {
    /// Type 0: samples on a grid, multilinear interpolation.
    Sampled {
        domain: Vec<f32>,
        range: Vec<f32>,
        size: Vec<usize>,
        encode: Vec<f32>,
        decode: Vec<f32>,
        /// Samples normalized to 0..1, laid out with the first input
        /// varying fastest, `outputs` values per grid point.
        samples: Vec<f32>,
        outputs: usize,
    },
    /// Type 2: `c0 + x^n (c1 - c0)`.
    Exponential {
        domain: Vec<f32>,
        c0: Vec<f32>,
        c1: Vec<f32>,
        n: f32,
    },
    /// Type 3: one of `parts` chosen by `bounds`, each with its own
    /// encoding of the sub-domain.
    Stitching {
        domain: Vec<f32>,
        parts: Vec<Function>,
        bounds: Vec<f32>,
        encode: Vec<f32>,
    },
    /// Type 4: a PostScript calculator program.
    PostScript {
        domain: Vec<f32>,
        range: Vec<f32>,
        program: Program,
    },
    /// An array of single-output functions sharing the same inputs.
    Array(Vec<Function>),
}

impl Function {
    /// Parse a function object: a dictionary, a stream, a reference to
    /// either, or an array of them.
    pub fn parse(doc: &Document, obj: &Object) -> Option<Function> {
        Self::parse_depth(doc, obj, 0)
    }

    fn parse_depth(doc: &Document, obj: &Object, depth: usize) -> Option<Function> {
        if depth > 8 {
            return None;
        }
        let obj = doc.dereference(obj).map(|(_, o)| o).unwrap_or(obj);
        match obj {
            Object::Array(items) => {
                let parts: Option<Vec<Function>> = items
                    .iter()
                    .map(|o| Self::parse_depth(doc, o, depth + 1))
                    .collect();
                Some(Function::Array(parts?))
            }
            Object::Dictionary(dict) => parse_dict(doc, dict, None, depth),
            Object::Stream(s) => {
                let data = s.decompressed_content_with_limit(MAX_SAMPLE_BYTES).ok()?;
                parse_dict(doc, &s.dict, Some(&data), depth)
            }
            _ => None,
        }
    }

    /// Evaluate on `inputs`, clipping to the domain and range.
    pub fn eval(&self, inputs: &[f32]) -> Option<Vec<f32>> {
        match self {
            Function::Array(parts) => parts
                .iter()
                .map(|f| f.eval(inputs).and_then(|v| v.first().copied()))
                .collect(),
            Function::Sampled { domain, range, .. } => {
                let x = clip(inputs, domain)?;
                Some(clip_range(sampled(self, &x)?, range))
            }
            Function::Exponential { domain, c0, c1, n } => {
                let x = clip(inputs, domain)?.first().copied()?;
                let t = if *n == 1.0 {
                    x
                } else {
                    x.abs().powf(*n) * x.signum()
                };
                Some(c0.iter().zip(c1).map(|(a, b)| a + t * (b - a)).collect())
            }
            Function::Stitching { .. } => self.stitch(inputs),
            Function::PostScript {
                domain,
                range,
                program,
            } => {
                let x = clip(inputs, domain)?;
                let out = program.eval(&x).ok()?;
                let n = range.len() / 2;
                let tail = out.get(out.len().checked_sub(n)?..)?;
                Some(clip_range(tail.to_vec(), range))
            }
        }
    }

    fn stitch(&self, inputs: &[f32]) -> Option<Vec<f32>> {
        let Function::Stitching {
            domain,
            parts,
            bounds,
            encode,
        } = self
        else {
            return None;
        };
        let x = clip(inputs, domain)?.first().copied()?;
        let k = bounds.iter().position(|b| x < *b).unwrap_or(bounds.len());
        let lo = if k == 0 { domain[0] } else { bounds[k - 1] };
        let hi = bounds.get(k).copied().unwrap_or(domain[1]);
        let e = encode.get(2 * k..2 * k + 2)?;
        parts
            .get(k)?
            .eval(&[interpolate(x, (lo, hi), (e[0], e[1]))])
    }

    pub fn outputs(&self) -> Option<usize> {
        match self {
            Function::Array(parts) => Some(parts.len()),
            Function::Sampled { outputs, .. } => Some(*outputs),
            Function::Exponential { c0, .. } => Some(c0.len()),
            Function::Stitching { parts, .. } => parts.first()?.outputs(),
            Function::PostScript { range, .. } => Some(range.len() / 2),
        }
    }
}

fn parse_dict(
    doc: &Document,
    dict: &Dictionary,
    data: Option<&[u8]>,
    depth: usize,
) -> Option<Function> {
    let domain = floats(doc, dict, b"Domain")?;
    let range = floats(doc, dict, b"Range");
    match dict.get(b"FunctionType").ok()?.as_i64().ok()? {
        0 => parse_sampled(doc, dict, data?, (domain, range?)),
        2 => Some(Function::Exponential {
            domain,
            c0: floats(doc, dict, b"C0").unwrap_or_else(|| vec![0.0]),
            c1: floats(doc, dict, b"C1").unwrap_or_else(|| vec![1.0]),
            n: dict.get(b"N").ok()?.as_float().ok()?,
        }),
        3 => {
            let functions = dict.get(b"Functions").ok()?;
            let functions = doc
                .dereference(functions)
                .map(|(_, o)| o)
                .unwrap_or(functions);
            let parts: Option<Vec<Function>> = functions
                .as_array()
                .ok()?
                .iter()
                .map(|o| Function::parse_depth(doc, o, depth + 1))
                .collect();
            Some(Function::Stitching {
                domain,
                parts: parts?,
                bounds: floats(doc, dict, b"Bounds")?,
                encode: floats(doc, dict, b"Encode")?,
            })
        }
        4 => Some(Function::PostScript {
            domain,
            range: range?,
            program: Program::parse(data?).ok()?,
        }),
        _ => None,
    }
}

fn parse_sampled(
    doc: &Document,
    dict: &Dictionary,
    data: &[u8],
    (domain, range): (Vec<f32>, Vec<f32>),
) -> Option<Function> {
    let size: Vec<usize> = floats(doc, dict, b"Size")?
        .iter()
        .map(|s| *s as usize)
        .collect();
    let bps = dict.get(b"BitsPerSample").ok()?.as_i64().ok()? as u32;
    if !matches!(bps, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32) || size.contains(&0) {
        return None;
    }
    let outputs = range.len() / 2;
    let points = size.iter().try_fold(1usize, |acc, s| acc.checked_mul(*s))?;
    let total = points.checked_mul(outputs)?;
    if total.checked_mul(bps as usize)?.div_ceil(8) > data.len() {
        return None;
    }
    let max = ((1u64 << bps) - 1) as f32;
    let mut pos = 0usize;
    let samples: Vec<f32> = (0..total)
        .map(|_| {
            let v = read_bits(data, pos, bps);
            pos += bps as usize;
            v as f32 / max
        })
        .collect();
    let encode = floats(doc, dict, b"Encode")
        .unwrap_or_else(|| size.iter().flat_map(|s| [0.0, (*s - 1) as f32]).collect());
    let decode = floats(doc, dict, b"Decode").unwrap_or_else(|| range.clone());
    Some(Function::Sampled {
        domain,
        range,
        size,
        encode,
        decode,
        samples,
        outputs,
    })
}

fn read_bits(data: &[u8], pos: usize, bits: u32) -> u64 {
    let mut v = 0u64;
    for i in 0..bits as usize {
        let p = pos + i;
        let byte = data.get(p / 8).copied().unwrap_or(0);
        v = (v << 1) | u64::from((byte >> (7 - p % 8)) & 1);
    }
    v
}

/// Multilinear interpolation on the sample grid.
fn sampled(f: &Function, x: &[f32]) -> Option<Vec<f32>> {
    let Function::Sampled {
        domain,
        size,
        encode,
        decode,
        samples,
        outputs,
        ..
    } = f
    else {
        return None;
    };
    let m = size.len();
    if x.len() < m || m > 16 {
        return None;
    }
    // Per input: integer cell and fraction within it.
    let mut cells = Vec::with_capacity(m);
    for i in 0..m {
        let e = interpolate(
            x[i],
            (domain[2 * i], domain[2 * i + 1]),
            (encode[2 * i], encode[2 * i + 1]),
        );
        let e = e.clamp(0.0, (size[i] - 1) as f32);
        let lo = (e.floor() as usize).min(size[i] - 1);
        cells.push((lo, e - lo as f32));
    }
    let mut out = vec![0.0f32; *outputs];
    for corner in 0..(1usize << m) {
        let mut weight = 1.0f32;
        let mut index = 0usize;
        let mut stride = 1usize;
        for (i, (lo, frac)) in cells.iter().enumerate() {
            let hi = (corner >> i) & 1 == 1;
            weight *= if hi { *frac } else { 1.0 - frac };
            let coord = if hi { (lo + 1).min(size[i] - 1) } else { *lo };
            index += coord * stride;
            stride *= size[i];
        }
        if weight == 0.0 {
            continue;
        }
        for (j, o) in out.iter_mut().enumerate() {
            *o += weight * samples.get(index * outputs + j).copied()?;
        }
    }
    out.iter()
        .enumerate()
        .map(|(j, v)| {
            let (dmin, dmax) = (decode.get(2 * j).copied()?, decode.get(2 * j + 1).copied()?);
            Some(dmin + v * (dmax - dmin))
        })
        .collect()
}

fn floats(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<Vec<f32>> {
    let obj = dict.get(key).ok()?;
    let obj = doc.dereference(obj).map(|(_, o)| o).unwrap_or(obj);
    obj.as_array()
        .ok()?
        .iter()
        .map(|o| o.as_float().ok())
        .collect()
}

fn interpolate(x: f32, (xmin, xmax): (f32, f32), (ymin, ymax): (f32, f32)) -> f32 {
    if xmax == xmin {
        ymin
    } else {
        ymin + (x - xmin) * (ymax - ymin) / (xmax - xmin)
    }
}

fn clip(inputs: &[f32], domain: &[f32]) -> Option<Vec<f32>> {
    let n = domain.len() / 2;
    if inputs.len() < n || n == 0 {
        return None;
    }
    Some(
        inputs[..n]
            .iter()
            .enumerate()
            .map(|(i, x)| {
                x.clamp(
                    domain[2 * i].min(domain[2 * i + 1]),
                    domain[2 * i + 1].max(domain[2 * i]),
                )
            })
            .collect(),
    )
}

fn clip_range(values: Vec<f32>, range: &[f32]) -> Vec<f32> {
    if range.len() < values.len() * 2 {
        return values;
    }
    values
        .into_iter()
        .enumerate()
        .map(|(j, v)| {
            v.clamp(
                range[2 * j].min(range[2 * j + 1]),
                range[2 * j + 1].max(range[2 * j]),
            )
        })
        .collect()
}

// ------------------------------------------------- type 4 calculator

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Op(&'static str),
    Block(Vec<Token>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Value {
    Num(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program(Vec<Token>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsError(String);

impl fmt::Display for PsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

const OPERATORS: [&str; 42] = [
    "abs", "add", "atan", "ceiling", "cos", "cvi", "cvr", "div", "exp", "floor", "idiv", "ln",
    "log", "mod", "mul", "neg", "round", "sin", "sqrt", "sub", "truncate", "and", "bitshift", "eq",
    "false", "ge", "gt", "le", "lt", "ne", "not", "or", "true", "xor", "if", "ifelse", "copy",
    "dup", "exch", "index", "pop", "roll",
];

impl Program {
    /// Parse the source text; the outermost braces are the program body.
    pub fn parse(source: &[u8]) -> Result<Program, PsError> {
        let text = String::from_utf8_lossy(source);
        let spaced = text.replace('{', " { ").replace('}', " } ");
        let mut words = spaced.split_whitespace();
        let body = parse_block(&mut words, 0)?;
        match body.as_slice() {
            [Token::Block(inner)] => Ok(Program(inner.clone())),
            _ => Err(PsError("program is not a single brace block".into())),
        }
    }

    /// Run the program on `inputs` and return the whole stack.
    pub fn eval(&self, inputs: &[f32]) -> Result<Vec<f32>, PsError> {
        let mut stack: Vec<Value> = inputs.iter().map(|&v| Value::Num(f64::from(v))).collect();
        run(&self.0, &mut stack, 0)?;
        stack
            .into_iter()
            .map(|v| match v {
                Value::Num(n) => Ok(n as f32),
                Value::Bool(_) => Err(PsError("boolean left on the stack".into())),
            })
            .collect()
    }
}

fn parse_block<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    depth: usize,
) -> Result<Vec<Token>, PsError> {
    let mut out = Vec::new();
    while let Some(word) = words.next() {
        match word {
            "{" if depth > 32 => return Err(PsError("braces nested too deeply".into())),
            "{" => out.push(Token::Block(parse_block(words, depth + 1)?)),
            "}" => return Ok(out),
            w => out.push(token(w)?),
        }
    }
    if depth == 0 {
        Ok(out)
    } else {
        Err(PsError("unbalanced braces".into()))
    }
}

fn token(word: &str) -> Result<Token, PsError> {
    if let Ok(n) = word.parse::<f64>() {
        return Ok(Token::Num(n));
    }
    // Radix numbers (16#FF) are allowed by PostScript; they are rare in PDF.
    if let Some((radix, digits)) = word.split_once('#')
        && let (Ok(r), Ok(v)) = (radix.parse::<u32>(), i64::from_str_radix(digits, 16))
        && r == 16
    {
        return Ok(Token::Num(v as f64));
    }
    OPERATORS
        .iter()
        .find(|op| **op == word)
        .map(|op| Token::Op(op))
        .ok_or_else(|| PsError(format!("unknown operator {word}")))
}

const MAX_STEPS: usize = 100_000;

fn run(tokens: &[Token], stack: &mut Vec<Value>, mut steps: usize) -> Result<usize, PsError> {
    let mut i = 0;
    while i < tokens.len() {
        steps += 1;
        if steps > MAX_STEPS {
            return Err(PsError("program runs too long".into()));
        }
        match &tokens[i] {
            Token::Num(n) => stack.push(Value::Num(*n)),
            Token::Block(_) => {
                let (consumed, branch) = conditional(&tokens[i..], stack)?;
                if let Some(body) = branch {
                    steps = run(body, stack, steps)?;
                }
                i += consumed - 1;
            }
            Token::Op(op) => apply(op, stack)?,
        }
        i += 1;
    }
    Ok(steps)
}

/// A block is always the operand of `if` or `ifelse`. Returns how many
/// tokens the construct spans and which block, if any, to execute.
fn conditional<'a>(
    tokens: &'a [Token],
    stack: &mut Vec<Value>,
) -> Result<(usize, Option<&'a [Token]>), PsError> {
    match tokens {
        [Token::Block(a), Token::Op("if"), ..] => {
            let cond = pop_bool(stack)?;
            Ok((2, cond.then_some(a.as_slice())))
        }
        [Token::Block(a), Token::Block(b), Token::Op("ifelse"), ..] => {
            let cond = pop_bool(stack)?;
            Ok((3, Some(if cond { a.as_slice() } else { b.as_slice() })))
        }
        _ => Err(PsError("block without if or ifelse".into())),
    }
}

fn pop(stack: &mut Vec<Value>) -> Result<Value, PsError> {
    stack.pop().ok_or_else(|| PsError("stack underflow".into()))
}

fn pop_num(stack: &mut Vec<Value>) -> Result<f64, PsError> {
    match pop(stack)? {
        Value::Num(n) => Ok(n),
        Value::Bool(_) => Err(PsError("number expected".into())),
    }
}

fn pop_bool(stack: &mut Vec<Value>) -> Result<bool, PsError> {
    match pop(stack)? {
        Value::Bool(b) => Ok(b),
        Value::Num(_) => Err(PsError("boolean expected".into())),
    }
}

fn pop_int(stack: &mut Vec<Value>) -> Result<i64, PsError> {
    Ok(pop_num(stack)?.trunc() as i64)
}

fn apply(op: &str, stack: &mut Vec<Value>) -> Result<(), PsError> {
    if stack.len() > 1000 {
        return Err(PsError("stack overflow".into()));
    }
    if let Some(f) = unary(op) {
        let a = pop_num(stack)?;
        stack.push(Value::Num(f(a)));
        return Ok(());
    }
    if let Some(f) = binary(op) {
        let b = pop_num(stack)?;
        let a = pop_num(stack)?;
        stack.push(Value::Num(f(a, b)));
        return Ok(());
    }
    if let Some(f) = comparison(op) {
        let b = pop(stack)?;
        let a = pop(stack)?;
        stack.push(Value::Bool(f(a, b)));
        return Ok(());
    }
    logical_or_stack(op, stack)
}

type Unary = fn(f64) -> f64;
type Binary = fn(f64, f64) -> f64;

const UNARY: &[(&str, Unary)] = &[
    ("abs", f64::abs),
    ("ceiling", f64::ceil),
    ("cos", |a| a.to_radians().cos()),
    ("cvi", f64::trunc),
    ("cvr", |a| a),
    ("floor", f64::floor),
    ("ln", f64::ln),
    ("log", f64::log10),
    ("neg", |a| -a),
    ("round", f64::round),
    ("sin", |a| a.to_radians().sin()),
    ("sqrt", f64::sqrt),
    ("truncate", f64::trunc),
];

const BINARY: &[(&str, Binary)] = &[
    ("add", |a, b| a + b),
    ("sub", |a, b| a - b),
    ("mul", |a, b| a * b),
    ("div", |a, b| a / b),
    ("idiv", |a, b| {
        (a.trunc() as i64)
            .checked_div(b.trunc() as i64)
            .unwrap_or(0) as f64
    }),
    ("mod", |a, b| {
        (a.trunc() as i64)
            .checked_rem(b.trunc() as i64)
            .unwrap_or(0) as f64
    }),
    ("exp", f64::powf),
    ("atan", |num, den| {
        let deg = num.atan2(den).to_degrees();
        if deg < 0.0 { deg + 360.0 } else { deg }
    }),
    ("bitshift", |a, s| {
        let (a, s) = (a.trunc() as i64, s.trunc() as i64);
        (if s >= 0 {
            a << s.min(63)
        } else {
            a >> (-s).min(63)
        }) as f64
    }),
];

fn unary(op: &str) -> Option<Unary> {
    UNARY.iter().find(|(name, _)| *name == op).map(|(_, f)| *f)
}

fn binary(op: &str) -> Option<Binary> {
    BINARY.iter().find(|(name, _)| *name == op).map(|(_, f)| *f)
}

fn comparison(op: &str) -> Option<fn(Value, Value) -> bool> {
    Some(match op {
        "eq" => |a, b| a == b,
        "ne" => |a, b| a != b,
        "gt" => |a, b| num(a) > num(b),
        "ge" => |a, b| num(a) >= num(b),
        "lt" => |a, b| num(a) < num(b),
        "le" => |a, b| num(a) <= num(b),
        _ => return None,
    })
}

fn num(v: Value) -> f64 {
    match v {
        Value::Num(n) => n,
        Value::Bool(b) => f64::from(u8::from(b)),
    }
}

/// `and`, `or`, `xor`, `not` act bitwise on integers and logically on
/// booleans; the rest are stack manipulation.
fn logical_or_stack(op: &str, stack: &mut Vec<Value>) -> Result<(), PsError> {
    match op {
        "true" => stack.push(Value::Bool(true)),
        "false" => stack.push(Value::Bool(false)),
        "not" => {
            let v = match pop(stack)? {
                Value::Bool(b) => Value::Bool(!b),
                Value::Num(n) => Value::Num(!(n.trunc() as i64) as f64),
            };
            stack.push(v);
        }
        "and" | "or" | "xor" => {
            let b = pop(stack)?;
            let a = pop(stack)?;
            stack.push(bitwise(op, a, b));
        }
        _ => stack_op(op, stack)?,
    }
    Ok(())
}

fn bitwise(op: &str, a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => Value::Bool(match op {
            "and" => x & y,
            "or" => x | y,
            _ => x ^ y,
        }),
        _ => {
            let (x, y) = (num(a).trunc() as i64, num(b).trunc() as i64);
            Value::Num(match op {
                "and" => x & y,
                "or" => x | y,
                _ => x ^ y,
            } as f64)
        }
    }
}

fn stack_op(op: &str, stack: &mut Vec<Value>) -> Result<(), PsError> {
    match op {
        "pop" => {
            pop(stack)?;
        }
        "dup" => {
            let v = pop(stack)?;
            stack.extend([v, v]);
        }
        "exch" => {
            let b = pop(stack)?;
            let a = pop(stack)?;
            stack.extend([b, a]);
        }
        "copy" => {
            let n = pop_int(stack)?.max(0) as usize;
            let start = stack
                .len()
                .checked_sub(n)
                .ok_or_else(|| PsError("copy underflow".into()))?;
            stack.extend_from_within(start..);
        }
        "index" => {
            let n = pop_int(stack)?.max(0) as usize;
            let v = *stack
                .iter()
                .rev()
                .nth(n)
                .ok_or_else(|| PsError("index underflow".into()))?;
            stack.push(v);
        }
        "roll" => {
            let j = pop_int(stack)?;
            let n = pop_int(stack)?.max(0) as usize;
            let start = stack
                .len()
                .checked_sub(n)
                .ok_or_else(|| PsError("roll underflow".into()))?;
            if n > 0 {
                stack[start..].rotate_right(j.rem_euclid(n as i64) as usize);
            }
        }
        other => return Err(PsError(format!("unhandled operator {other}"))),
    }
    Ok(())
}

#[cfg(test)]
mod postscript_tests {
    use super::*;

    fn eval(src: &str, inputs: &[f32]) -> Vec<f32> {
        Program::parse(src.as_bytes())
            .unwrap()
            .eval(inputs)
            .unwrap()
    }

    #[test]
    fn arithmetic_and_stack_operators() {
        assert_eq!(eval("{ add 2 mul }", &[1.0, 2.0]), vec![6.0]);
        assert_eq!(
            eval("{ dup mul exch dup mul add sqrt }", &[3.0, 4.0]),
            vec![5.0]
        );
        assert_eq!(eval("{ 3 1 roll }", &[1.0, 2.0, 3.0]), vec![3.0, 1.0, 2.0]);
        assert_eq!(eval("{ 3 -1 roll }", &[1.0, 2.0, 3.0]), vec![2.0, 3.0, 1.0]);
        assert_eq!(eval("{ 2 copy }", &[1.0, 2.0]), vec![1.0, 2.0, 1.0, 2.0]);
        assert_eq!(eval("{ 1 index }", &[7.0, 8.0]), vec![7.0, 8.0, 7.0]);
        assert_eq!(eval("{ 7 3 idiv 7 3 mod }", &[]), vec![2.0, 1.0]);
        assert_eq!(eval("{ 90 sin 1 1 atan }", &[]), vec![1.0, 45.0]);
    }

    #[test]
    fn conditionals_and_booleans() {
        // A typical tint transform: 1 - x for a gray alternate.
        assert_eq!(eval("{ 1 exch sub }", &[0.25]), vec![0.75]);
        assert_eq!(
            eval("{ dup 0.5 gt { pop 1 } { pop 0 } ifelse }", &[0.7]),
            vec![1.0]
        );
        assert_eq!(
            eval("{ dup 0.5 gt { pop 1 } { pop 0 } ifelse }", &[0.2]),
            vec![0.0]
        );
        assert_eq!(eval("{ dup 0.5 gt { 2 mul } if }", &[0.6]), vec![1.2]);
        assert_eq!(eval("{ true false or { 1 } { 0 } ifelse }", &[]), vec![1.0]);
        assert_eq!(eval("{ 6 3 and 6 3 xor 1 not }", &[]), vec![2.0, 5.0, -2.0]);
    }

    #[test]
    fn malformed_programs_are_errors() {
        assert!(Program::parse(b"{ add").is_err());
        assert!(Program::parse(b"{ frobnicate }").is_err());
        assert!(Program::parse(b"1 2 add").is_err());
        assert!(Program::parse(b"{ add }").unwrap().eval(&[1.0]).is_err());
        assert!(Program::parse(b"{ true }").unwrap().eval(&[]).is_err());
    }
}

#[cfg(test)]
mod tests {
    use lopdf::{Stream, dictionary};

    use super::*;

    fn close(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn exponential_and_stitching() {
        let doc = Document::with_version("1.5");
        let f = Function::parse(
            &doc,
            &Object::Dictionary(
                dictionary! { "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()],
                "C0" => vec![1.into(), 0.into()], "C1" => vec![0.into(), 1.into()], "N" => 1 },
            ),
        )
        .unwrap();
        assert!(close(&f.eval(&[0.25]).unwrap(), &[0.75, 0.25]));
        assert_eq!(f.outputs(), Some(2));
        let stitched = Function::parse(
            &doc,
            &Object::Dictionary(dictionary! { "FunctionType" => 3, "Domain" => vec![0.into(), 1.into()],
                "Functions" => vec![
                    Object::Dictionary(dictionary! { "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()], "C0" => vec![0.into()], "C1" => vec![1.into()], "N" => 1 }),
                    Object::Dictionary(dictionary! { "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()], "C0" => vec![1.into()], "C1" => vec![0.into()], "N" => 1 }),
                ],
                "Bounds" => vec![0.5.into()], "Encode" => vec![0.into(), 1.into(), 0.into(), 1.into()] }),
        )
        .unwrap();
        assert!(close(&stitched.eval(&[0.25]).unwrap(), &[0.5]));
        assert!(close(&stitched.eval(&[0.75]).unwrap(), &[0.5]));
        assert!(close(&stitched.eval(&[0.5]).unwrap(), &[1.0]));
    }

    #[test]
    fn sampled_interpolates_between_grid_points() {
        let doc = Document::with_version("1.5");
        // 1 input, 2 outputs, 3 samples: (0,255) (128,128) (255,0).
        let f = Function::parse(
            &doc,
            &Object::Stream(Stream::new(
                dictionary! { "FunctionType" => 0, "Domain" => vec![0.into(), 1.into()],
                "Range" => vec![0.into(), 1.into(), 0.into(), 1.into()],
                "Size" => vec![3.into()], "BitsPerSample" => 8 },
                vec![0, 255, 128, 128, 255, 0],
            )),
        )
        .unwrap();
        assert!(close(&f.eval(&[0.0]).unwrap(), &[0.0, 1.0]));
        assert!(close(&f.eval(&[0.25]).unwrap(), &[0.251, 0.751]));
        assert!(close(&f.eval(&[1.0]).unwrap(), &[1.0, 0.0]));
        // 2 inputs, 1 output, 2x2 grid: bilinear over the corners 0,1,1,0.
        let g = Function::parse(
            &doc,
            &Object::Stream(Stream::new(
                dictionary! { "FunctionType" => 0, "Domain" => vec![0.into(), 1.into(), 0.into(), 1.into()],
                    "Range" => vec![0.into(), 1.into()], "Size" => vec![2.into(), 2.into()], "BitsPerSample" => 8 },
                vec![0, 255, 255, 0],
            )),
        )
        .unwrap();
        assert!(close(&g.eval(&[0.5, 0.5]).unwrap(), &[0.5]));
        assert!(close(&g.eval(&[1.0, 0.0]).unwrap(), &[1.0]));
    }

    #[test]
    fn postscript_and_arrays() {
        let doc = Document::with_version("1.5");
        let f = Function::parse(
            &doc,
            &Object::Stream(Stream::new(
                dictionary! { "FunctionType" => 4, "Domain" => vec![0.into(), 1.into()],
                    "Range" => vec![0.into(), 1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into(), 1.into()] },
                b"{ dup 0.5 mul exch 0 exch 0 exch }".to_vec(),
            )),
        )
        .unwrap();
        assert!(close(&f.eval(&[0.8]).unwrap(), &[0.4, 0.0, 0.0, 0.8]));
        let arr = Function::parse(
            &doc,
            &Object::Array(vec![
                Object::Dictionary(dictionary! { "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()], "C0" => vec![0.into()], "C1" => vec![1.into()], "N" => 1 }),
                Object::Dictionary(dictionary! { "FunctionType" => 2, "Domain" => vec![0.into(), 1.into()], "C0" => vec![1.into()], "C1" => vec![0.into()], "N" => 2 }),
            ]),
        )
        .unwrap();
        assert!(close(&arr.eval(&[0.5]).unwrap(), &[0.5, 0.75]));
    }
}
