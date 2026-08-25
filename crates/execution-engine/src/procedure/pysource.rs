//! Static scan of a Python source file, for the procedure lint.
//!
//! A line scan rather than a parser, deliberately — and the SAME scan the
//! Studio Inspector runs in the browser (`apps/web/components/studio/ide/
//! python-target.ts`): the file may not be valid Python while it is being
//! written, nothing here imports it (importing a plug module can open a
//! serial port), and the two surfaces must agree on what "the class is
//! not in this file" means. Keep the grammar in step with the TypeScript.
//!
//! Comments are not parsed, and neither are strings at column 0, so a
//! `def` inside a triple-quoted block at the top level is a false
//! positive both sides accept. Inside a class body they ARE tracked
//! (`TripleQuotes`): a docstring there is the norm, and its Google-style
//! `Attributes:` section is indistinguishable from annotated fields.
//!
//! `binds_top_level` — the only scan here that BLOCKS a run — reads
//! LOGICAL lines instead: a statement continued by an open bracket or a
//! trailing backslash is one line, and a binding is top-level when no
//! `def`/`class` encloses it rather than when it starts at column 0.
//! Five ordinary spellings are how plug modules are actually written —
//! a parenthesized `from x import (\n  A,\n)`, a backslash continuation,
//! an import inside `try/except ImportError`, a class inside `if
//! sys.platform == ...`, a `class Foo(` whose bases are on the next line
//! — and every one of them used to read as "the symbol is not in this
//! file". The extractors stay column-0: they fill dropdowns, where a
//! conditional definition missing is a smaller cost than the Inspector
//! and the engine disagreeing about the same file.

use regex::Regex;
use std::sync::OnceLock;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

/// A top-level `def` / `async def`. Column 0 only: an indented `def` is a
/// method or a closure, not an importable callable.
fn def_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"^(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")
}

/// A top-level `class Name:` or `class Name(Base):`; a nested class is not
/// addressable as `module:Name`. Capture 2 is the base list, so the
/// closing `)` and `:` have to be on the line — only use this where the
/// bases are wanted, `class_header()` otherwise.
fn class_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"^class\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:\(([^)]*)\))?\s*:")
}

/// The start of a class statement, name only. A long base list moves the
/// `):` to a later line, and the class is no less defined for it — the
/// Inspector's `CLASS_LINE` (`python-target.ts`) has always read it this
/// way, and `class_line()` requiring the colon is the one grammar the two
/// surfaces disagreed on.
fn class_header() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"^class\s+([A-Za-z_][A-Za-z0-9_]*)\s*[(:]")
}

/// A top-level binding that is neither `def` nor `class` but still makes
/// `module:Name` importable: a re-export (`from .impl import Name`,
/// `import x as Name`) or a factory result (`Name = make_plug(...)`).
/// The Inspector's dropdowns do not list these, but a lint that BLOCKS a
/// run must not call a working reference broken, so they count as
/// defined here.
fn import_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"^(?:from\s+\S+\s+)?import\s+(.+)$")
}

/// One Python statement, however many physical lines it was written over.
struct LogicalLine {
    /// Indentation of the first physical line, in characters.
    indent: usize,
    /// The statement, comments stripped and continuations joined by a
    /// single space, trimmed.
    text: String,
}

/// Bracket depth and open triple-quote, carried from one physical line to
/// the next. Enough to tell where a statement really ends; not a lexer.
#[derive(Default)]
struct Continuation {
    depth: i32,
    triple: Option<char>,
}

impl Continuation {
    /// Consume one physical line. Returns its code — the part before any
    /// `#` comment — and whether a trailing backslash continues it.
    fn feed(&mut self, line: &str) -> (String, bool) {
        let c: Vec<char> = line.chars().collect();
        let mut single: Option<char> = None;
        let mut end = c.len();
        let mut i = 0;
        while i < c.len() {
            let ch = c[i];
            if let Some(q) = self.triple {
                if ch == q && c.get(i + 1) == Some(&q) && c.get(i + 2) == Some(&q) {
                    self.triple = None;
                    i += 3;
                } else {
                    i += if ch == '\\' { 2 } else { 1 };
                }
                continue;
            }
            if let Some(q) = single {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if ch == q {
                    single = None;
                }
                i += 1;
                continue;
            }
            if ch == '#' {
                end = i;
                break;
            }
            if ch == '"' || ch == '\'' {
                if c.get(i + 1) == Some(&ch) && c.get(i + 2) == Some(&ch) {
                    self.triple = Some(ch);
                    i += 3;
                } else {
                    single = Some(ch);
                    i += 1;
                }
                continue;
            }
            if "([{".contains(ch) {
                self.depth += 1;
            }
            if ")]}".contains(ch) {
                // Saturating: an unbalanced `)` in a string we mis-read
                // must not leave the scan permanently negative.
                self.depth = (self.depth - 1).max(0);
            }
            i += 1;
        }
        let code: String = c[..end].iter().collect();
        let backslash = self.triple.is_none() && code.trim_end().ends_with('\\');
        (code, backslash)
    }

    /// Whether the statement is complete at the end of the line just fed.
    fn settled(&self, backslash: bool) -> bool {
        self.depth == 0 && self.triple.is_none() && !backslash
    }
}

/// The file as statements. A triple-quoted block collapses into the
/// logical line that opened it, so a `def` inside a docstring cannot be
/// mistaken for a definition here.
fn logical_lines(content: &str) -> Vec<LogicalLine> {
    let mut out: Vec<LogicalLine> = Vec::new();
    let mut cont = Continuation::default();
    let mut open: Option<LogicalLine> = None;
    for raw in content.lines() {
        let (code, backslash) = cont.feed(raw);
        let piece = code.trim().trim_end_matches('\\').trim_end();
        match open.as_mut() {
            Some(line) => {
                if !piece.is_empty() {
                    if !line.text.is_empty() {
                        line.text.push(' ');
                    }
                    line.text.push_str(piece);
                }
            }
            None => {
                open = Some(LogicalLine {
                    indent: code.len() - code.trim_start().len(),
                    text: piece.to_string(),
                });
            }
        }
        if cont.settled(backslash) {
            if let Some(line) = open.take() {
                if !line.text.is_empty() {
                    out.push(line);
                }
            }
        }
    }
    // Unterminated bracket or quote at EOF — the file is mid-edit.
    if let Some(line) = open {
        if !line.text.is_empty() {
            out.push(line);
        }
    }
    out
}

/// `def __init__(` in a class body: indented.
fn init_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"^\s+(?:async\s+)?def\s+__init__\s*\(")
}

/// Any indented method — ends a dataclass-style attribute block.
fn method_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"^\s+(?:async\s+)?def\s")
}

/// An annotated class attribute: `name: Type` or `name: Type = default`.
/// Only annotated ones count — a bare `x = 1` is a class variable, not a
/// dataclass / pydantic field.
fn attribute_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(
        &RE,
        r"^\s+([A-Za-z_][A-Za-z0-9_]*)\s*:\s*([^=]+?)\s*(?:=\s*(.+?))?\s*$",
    )
}

/// Triple-quoted blocks in a class body, tracked across a line scan.
///
/// A Google-style docstring is the house style in most Python projects,
/// and its `Attributes:` section reads exactly like annotated fields —
/// `address: the VISA address` matches `attribute_line()`, and so does
/// `ValueError: if bad` under `Raises:`. Walking a `@dataclass` body
/// without this invents required keys the constructor never had, and
/// `certain: true` turns each one into an Error there is no `config:`
/// the author can write to satisfy.
///
/// Only the class-body scans use it, not the whole-file ones: an
/// unterminated quote while the file is being typed leaves this open to
/// EOF, which here ends in "no attributes" — `None`, issue nothing —
/// whereas in `binds_top_level` it would call a working class missing.
#[derive(Default)]
struct TripleQuotes {
    open: Option<&'static str>,
}

impl TripleQuotes {
    /// Consumes the line's delimiters and reports whether it is string
    /// content rather than code. A line carrying a delimiter counts as
    /// string on either side of it — the docstring's own first and last
    /// lines are never fields.
    fn is_string(&mut self, line: &str) -> bool {
        let mut carries = self.open.is_some();
        let mut rest = line;
        loop {
            match self.open {
                Some(delim) => match rest.find(delim) {
                    Some(i) => {
                        self.open = None;
                        rest = &rest[i + delim.len()..];
                    }
                    None => return true,
                },
                None => {
                    let opener = ["\"\"\"", "'''"]
                        .into_iter()
                        .filter_map(|d| rest.find(d).map(|i| (i, d)))
                        .min_by_key(|(i, _)| *i);
                    match opener {
                        Some((i, delim)) => {
                            carries = true;
                            self.open = Some(delim);
                            rest = &rest[i + delim.len()..];
                        }
                        None => return carries,
                    }
                }
            }
        }
    }
}

fn identifier() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re(&RE, r"^[A-Za-z_][A-Za-z0-9_]*$")
}

/// Top-level `def` / `async def` names, in source order, deduped.
pub fn top_level_functions(content: &str) -> Vec<String> {
    top_level_names(content, def_line())
}

/// Top-level classes, in file order, deduped.
pub fn top_level_classes(content: &str) -> Vec<String> {
    top_level_names(content, class_header())
}

fn top_level_names(content: &str, pattern: &Regex) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in content.lines() {
        if let Some(name) = pattern.captures(line).and_then(|c| c.get(1)) {
            let name = name.as_str();
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Whether `module:name` can resolve in this file without importing it:
/// a module-level `def`/`class` of that name, or a module-level import or
/// assignment that binds it. `false` is the lint's "not found in file",
/// and the caller turns that into an Error that refuses to start the run
/// — so every doubt resolves to `true`. A star import makes the whole
/// file undecidable and answers `true` for any name.
///
/// Module level is "nothing encloses it", not "column 0": a class under
/// `if sys.platform == "win32":` and an import under `except
/// ImportError:` bind exactly as if they were unindented. Only a `def`
/// or a `class` opens a namespace that hides what is inside it.
pub fn binds_top_level(content: &str, name: &str) -> bool {
    // Indentation of the outermost enclosing `def`/`class`, while inside
    // one. Its body is a namespace of its own: a method is not importable
    // as `module:name`.
    let mut namespace: Option<usize> = None;
    for line in logical_lines(content) {
        if namespace.is_some_and(|indent| line.indent <= indent) {
            namespace = None;
        }
        if namespace.is_some() {
            continue;
        }
        let text = line.text.as_str();
        if let Some(c) = def_line()
            .captures(text)
            .or_else(|| class_header().captures(text))
        {
            namespace = Some(line.indent);
            if &c[1] == name {
                return true;
            }
            continue;
        }
        if let Some(c) = import_line().captures(text) {
            // `import a.b as Name`, `from m import A, B as Name`,
            // `from m import (A, B)` — the bound name is the last
            // identifier of each comma part.
            let imported = c[1].trim().trim_start_matches('(').trim_end_matches(')');
            for part in imported.split(',') {
                let part = part.trim();
                // `from .impl import *`: the module re-exports names this
                // scan cannot see. Nothing is provably absent after it.
                if part == "*" {
                    return true;
                }
                let bound = part
                    .split_whitespace()
                    .last()
                    .unwrap_or("")
                    .rsplit('.')
                    .next()
                    .unwrap_or("");
                if bound == name {
                    return true;
                }
            }
            continue;
        }
        if binds_assignment(text, name) {
            return true;
        }
    }
    false
}

/// Whether an assignment statement binds `name`: `Name = ...`,
/// `Name: T = ...`, and the tuple form `Other, Name = ...`.
fn binds_assignment(text: &str, name: &str) -> bool {
    let Some(eq) = default_split(text) else {
        return false;
    };
    split_top_level(&text[..eq]).iter().any(|target| {
        let target = target.trim().trim_start_matches('(').trim_end_matches(')');
        let target = match target.find(':') {
            Some(i) => &target[..i],
            None => target,
        };
        target.trim() == name
    })
}

/// One keyword argument a plug class accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitParam {
    pub name: String,
    /// No default: omitting it from `config:` is a `TypeError` at
    /// instantiation.
    pub required: bool,
}

/// Where the keywords were read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureSource {
    /// The class's own `def __init__` — authoritative.
    Init,
    /// Annotated class attributes, the `@dataclass` / pydantic form where
    /// there is no `__init__` at all. Also what a plain class inheriting
    /// `__init__` from a base in another file looks like, so a mismatch
    /// against this is a hint, not a certainty.
    Attributes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitSignature {
    pub params: Vec<InitParam>,
    /// The class takes `**kwargs`, so no key can be "unknown".
    pub kwargs: bool,
    pub source: SignatureSource,
    /// Whether a mismatch against `params` is a certain `TypeError` at
    /// instantiation. True for a `def __init__` and for the generated
    /// constructors we can see being generated — a `@dataclass` / attrs
    /// decorator, or a pydantic `BaseModel` base. False for a plain class
    /// whose annotated attributes may or may not be its constructor
    /// (inherited `__init__` in another file, or no keyword constructor
    /// at all); callers report those as warnings.
    pub certain: bool,
}

/// The keyword arguments a plug class accepts, read out of its own
/// source. `None` when they cannot be determined — a class inheriting
/// `__init__` from a base in another file looks identical to one taking
/// no arguments from inside this file; reading it as "accepts nothing"
/// would flag every working key as unknown. `None` means: issue nothing.
pub fn init_signature(content: &str, class_name: &str) -> Option<InitSignature> {
    let lines: Vec<&str> = content.lines().collect();
    let class_idx = lines.iter().position(|l| {
        class_line()
            .captures(l)
            .map(|c| &c[1] == class_name)
            .unwrap_or(false)
    })?;

    // `def __init__(` in the class body: indented, and before the next
    // column-0 statement (which ends the class).
    let mut init_idx = None;
    let mut quotes = TripleQuotes::default();
    for (i, raw) in lines.iter().enumerate().skip(class_idx + 1) {
        if raw.trim().is_empty() {
            continue;
        }
        if quotes.is_string(raw) {
            continue;
        }
        if !raw.starts_with(|c: char| c.is_whitespace()) {
            break;
        }
        if init_line().is_match(raw) {
            init_idx = Some(i);
            break;
        }
    }

    match init_idx {
        Some(i) => {
            let (params, kwargs) = parse_signature_at(&lines, i);
            Some(InitSignature {
                params,
                kwargs,
                source: SignatureSource::Init,
                certain: true,
            })
        }
        None => class_attribute_params(&lines, class_idx),
    }
}

/// Whether the class at `class_idx` visibly generates a keyword
/// constructor from its annotated attributes: a `@dataclass` (bare, called,
/// or module-qualified), an attrs decorator, or a pydantic `BaseModel`
/// base. Decorators are the column-0 `@` lines directly above the class.
fn generates_constructor(lines: &[&str], class_idx: usize) -> bool {
    let bases = class_line()
        .captures(lines[class_idx])
        .and_then(|c| c.get(2))
        .map(|m| m.as_str())
        .unwrap_or("");
    if bases.contains("BaseModel") {
        return true;
    }
    let mut i = class_idx;
    while i > 0 {
        i -= 1;
        let line = lines[i];
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with('@') {
            break;
        }
        let deco = line.trim_start_matches('@');
        if deco.contains("dataclass") || deco.contains("attr.s") || deco.contains("attrs.") || deco.starts_with("define") {
            return true;
        }
    }
    false
}

/// The parameters of the `def` starting at `def_idx`. Accumulates the
/// signature until the parens balance, so a definition split over several
/// lines reads the same as a one-liner; quote-aware so a paren inside a
/// default string does not move the depth.
fn parse_signature_at(lines: &[&str], def_idx: usize) -> (Vec<InitParam>, bool) {
    let mut sig = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut closed = false;
    for line in lines.iter().skip(def_idx) {
        for ch in line.chars() {
            if let Some(q) = quote {
                sig.push(ch);
                if ch == q {
                    quote = None;
                }
                continue;
            }
            if ch == '"' || ch == '\'' {
                quote = Some(ch);
                if depth >= 1 {
                    sig.push(ch);
                }
                continue;
            }
            if ch == '(' {
                depth += 1;
                if depth == 1 {
                    continue;
                }
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 {
                    closed = true;
                    break;
                }
            }
            if depth >= 1 {
                sig.push(ch);
            }
        }
        if closed {
            break;
        }
        // A newline inside the signature is whitespace between parameters.
        sig.push(' ');
    }

    let mut params = Vec::new();
    let mut kwargs = false;
    for raw in split_top_level(&sig) {
        // `self`, the positional-only marker and the keyword-only marker
        // carry no keyword of their own.
        if raw == "self" || raw == "/" || raw == "*" {
            continue;
        }
        if raw.starts_with("**") {
            kwargs = true;
            continue;
        }
        // `*args` collects positionals; a keyword can never reach it.
        if raw.starts_with('*') {
            continue;
        }
        if let Some(p) = parse_param(&raw) {
            params.push(p);
        }
    }
    (params, kwargs)
}

/// Split a parameter list on top-level commas only: a default like
/// `[1, 2, 3]` or `Dict[str, int]` is one parameter, not three.
fn split_top_level(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    for ch in src.chars() {
        if let Some(q) = quote {
            cur.push(ch);
            if ch == q {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            cur.push(ch);
            continue;
        }
        if "([{".contains(ch) {
            depth += 1;
        }
        if ")]}".contains(ch) {
            depth -= 1;
        }
        if ch == ',' && depth == 0 {
            out.push(std::mem::take(&mut cur));
            continue;
        }
        cur.push(ch);
    }
    out.push(cur);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Index of the `=` separating name from default, skipping `=` inside a
/// default's own brackets or string, and `:=` / `==`.
fn default_split(param: &str) -> Option<usize> {
    let chars: Vec<char> = param.chars().collect();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, &ch) in chars.iter().enumerate() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if "([{".contains(ch) {
            depth += 1;
        }
        if ")]}".contains(ch) {
            depth -= 1;
        }
        if ch == '='
            && depth == 0
            && (i == 0 || chars[i - 1] != ':')
            && chars.get(i + 1) != Some(&'=')
        {
            // Byte index of this char, for slicing.
            return Some(param.char_indices().nth(i).map(|(b, _)| b).unwrap_or(0));
        }
    }
    None
}

fn parse_param(param: &str) -> Option<InitParam> {
    let eq = default_split(param);
    let head = match eq {
        Some(i) => &param[..i],
        None => param,
    }
    .trim();
    let has_default = match eq {
        Some(i) => !param[i + 1..].trim().is_empty(),
        None => false,
    };
    let name = match head.find(':') {
        Some(c) => &head[..c],
        None => head,
    }
    .trim();
    if !identifier().is_match(name) {
        return None;
    }
    Some(InitParam {
        name: name.to_string(),
        required: !has_default,
    })
}

/// A class with no `__init__` may still declare its keywords as annotated
/// class attributes (`@dataclass`, pydantic); the generated constructor
/// takes exactly those.
fn class_attribute_params(lines: &[&str], class_idx: usize) -> Option<InitSignature> {
    let mut params = Vec::new();
    let mut quotes = TripleQuotes::default();
    for raw in lines.iter().skip(class_idx + 1) {
        if raw.trim().is_empty() {
            continue;
        }
        // A docstring's `Attributes:` / `Raises:` entries look exactly
        // like annotated fields; the block is string, not code.
        if quotes.is_string(raw) {
            continue;
        }
        if !raw.starts_with(|c: char| c.is_whitespace()) {
            break;
        }
        // The attribute block ends at the first method.
        if method_line().is_match(raw) {
            break;
        }
        let Some(c) = attribute_line().captures(raw) else {
            continue;
        };
        params.push(InitParam {
            name: c[1].to_string(),
            required: c.get(3).is_none(),
        });
    }
    // No annotated attributes either: the constructor is genuinely
    // unreadable from this file (inherited, or built dynamically).
    if params.is_empty() {
        None
    } else {
        Some(InitSignature {
            params,
            kwargs: false,
            source: SignatureSource::Attributes,
            certain: generates_constructor(lines, class_idx),
        })
    }
}

/// The declared parameter an unknown key was probably meant to be: the
/// CLOSEST within two edits, case-insensitive — same threshold and same
/// tie-break as the Inspector, so both surfaces suggest the same name or
/// none. Declaration order is not closeness: with `__init__(self, porta,
/// port)`, the key `prot` is one edit from `port` and two from `porta`.
pub fn suggestion_for<'a>(key: &str, params: &'a [InitParam]) -> Option<&'a str> {
    let key = key.to_lowercase();
    params
        .iter()
        .filter_map(|p| edit_distance(&key, &p.name.to_lowercase()).map(|d| (d, p)))
        .min_by_key(|(d, _)| *d)
        .map(|(_, p)| p.name.as_str())
}

/// Levenshtein distance, `None` past two edits: anything further is not
/// a typo and the answer is the same as "no idea".
fn edit_distance(a: &str, b: &str) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if (a.len() as i64 - b.len() as i64).abs() > 2 {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur.push((prev[j] + 1).min(cur[j - 1] + 1).min(sub));
        }
        prev = cur;
    }
    Some(prev[b.len()]).filter(|d| *d <= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUG: &str = r#"
import serial
from .base import BasePlug, helper as make

class PowerSupply(BasePlug):
    """A PSU."""

    def __init__(self, address: str, port: int = 5025, *, sep: str = "a(b", **_ignored):
        self.address = address

    def measure(self):
        pass

def make_default():
    return PowerSupply("x")

Alias = make(PowerSupply)
"#;

    #[test]
    fn top_level_names_follow_the_column_zero_rule() {
        assert_eq!(top_level_classes(PLUG), vec!["PowerSupply"]);
        assert_eq!(top_level_functions(PLUG), vec!["make_default"]);
    }

    #[test]
    fn binds_top_level_counts_defs_classes_imports_and_assignments() {
        assert!(binds_top_level(PLUG, "PowerSupply"));
        assert!(binds_top_level(PLUG, "make_default"));
        assert!(
            binds_top_level(PLUG, "BasePlug"),
            "re-export via from-import"
        );
        assert!(binds_top_level(PLUG, "make"), "import ... as");
        assert!(binds_top_level(PLUG, "serial"));
        assert!(binds_top_level(PLUG, "Alias"), "factory assignment");
        assert!(
            !binds_top_level(PLUG, "measure"),
            "a method is not importable"
        );
        assert!(!binds_top_level(PLUG, "PowerSuply"));
    }

    /// Every one of these is a working plug module that the column-0 scan
    /// called broken, and `RefSeverity::Error` refuses to start the run —
    /// no hardware touched, no override.
    #[test]
    fn binds_top_level_reads_the_ordinary_import_spellings() {
        assert!(
            binds_top_level(
                "from .base import (\n    BasePlug,\n    PowerSupply,\n)\n",
                "PowerSupply"
            ),
            "parenthesized import, the spelling a list grows into"
        );
        assert!(
            binds_top_level(
                "from .base import BasePlug, \\\n    PowerSupply\n",
                "PowerSupply"
            ),
            "backslash continuation"
        );
        assert!(
            binds_top_level("from .impl import *\n", "PowerSupply"),
            "a star import re-exports names this scan cannot see"
        );
        assert!(
            binds_top_level(
                "try:\n    from .fast import PowerSupply\nexcept ImportError:\n    from .slow import PowerSupply\n",
                "PowerSupply"
            ),
            "an optional dependency binds at module level from inside try"
        );
        assert!(
            binds_top_level(
                "import sys\n\nif sys.platform == \"win32\":\n    class PowerSupply:\n        pass\nelse:\n    class PowerSupply:\n        pass\n",
                "PowerSupply"
            ),
            "a conditional class is still a module attribute"
        );
        assert!(
            binds_top_level(
                "class PowerSupply(\n    BasePlug,\n):\n    pass\n",
                "PowerSupply"
            ),
            "a base list on the next line — what the Inspector always accepted"
        );
        assert!(
            binds_top_level("PowerSupply, Meter = build_pair()\n", "Meter"),
            "tuple unpacking binds both names"
        );
    }

    /// Leniency has to stop somewhere, or the lint reports nothing.
    #[test]
    fn binds_top_level_still_refuses_what_is_not_a_module_attribute() {
        assert!(
            !binds_top_level(
                "def build():\n    class PowerSupply:\n        pass\n    return PowerSupply\n",
                "PowerSupply"
            ),
            "a class inside a function is not importable as module:Name"
        );
        assert!(
            !binds_top_level(
                "class Rig:\n    class PowerSupply:\n        pass\n",
                "PowerSupply"
            ),
            "a nested class is addressed as Rig.PowerSupply, not module:PowerSupply"
        );
        assert!(
            !binds_top_level(
                "\"\"\"Docs.\n\nclass PowerSupply:\n    an example, not code\n\"\"\"\n",
                "PowerSupply"
            ),
            "a class in the module docstring defines nothing"
        );
        assert!(
            !binds_top_level("from .base import BasePlug  # PowerSupply\n", "PowerSupply"),
            "a comment defines nothing"
        );
    }

    #[test]
    fn top_level_classes_read_a_multi_line_base_list() {
        assert_eq!(
            top_level_classes("class PowerSupply(\n    BasePlug,\n):\n    pass\n"),
            vec!["PowerSupply"],
            "same grammar as the Inspector's CLASS_LINE"
        );
    }

    #[test]
    fn init_signature_reads_the_constructor() {
        let sig = init_signature(PLUG, "PowerSupply").expect("signature");
        assert_eq!(sig.source, SignatureSource::Init);
        assert!(sig.kwargs);
        let names: Vec<(&str, bool)> = sig
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.required))
            .collect();
        assert_eq!(
            names,
            vec![("address", true), ("port", false), ("sep", false)]
        );
    }

    #[test]
    fn init_signature_spans_lines_and_nested_defaults() {
        let src = "class A:\n    def __init__(\n        self,\n        limits: dict = {\"a\": (1, 2)},\n        name: str,\n    ):\n        pass\n";
        let sig = init_signature(src, "A").unwrap();
        let names: Vec<(&str, bool)> = sig
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.required))
            .collect();
        assert_eq!(names, vec![("limits", false), ("name", true)]);
        assert!(!sig.kwargs);
    }

    #[test]
    fn init_signature_falls_back_to_annotated_attributes() {
        let src = "@dataclass\nclass Cfg:\n    address: str\n    port: int = 1\n    count = 3\n\n    def go(self):\n        pass\n";
        let sig = init_signature(src, "Cfg").unwrap();
        assert_eq!(sig.source, SignatureSource::Attributes);
        assert!(sig.certain, "@dataclass generates the constructor");
        let names: Vec<(&str, bool)> = sig
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.required))
            .collect();
        assert_eq!(names, vec![("address", true), ("port", false)]);
    }

    #[test]
    fn docstring_sections_are_not_fields() {
        // Google style: the `Attributes:` and `Raises:` entries read
        // exactly like annotated attributes, and `certain` would make
        // each invented one a blocking Error.
        let src = r#"
@dataclass
class Cfg:
    """A PSU config.

    Attributes:
        address: the VISA address
    Raises:
        ValueError: if bad
    """

    address: str = "192.168.1.1"
    port: int = 5025
"#;
        let sig = init_signature(src, "Cfg").expect("signature");
        assert!(sig.certain);
        let names: Vec<(&str, bool)> = sig
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.required))
            .collect();
        assert_eq!(names, vec![("address", false), ("port", false)]);
    }

    #[test]
    fn docstring_neither_hides_nor_invents_an_init() {
        // A constructor shown in a docstring is not the constructor.
        let doc_only = "class Cfg:\n    '''Usage:\n\n    def __init__(self, nope):\n    '''\n\n    address: str\n";
        let sig = init_signature(doc_only, "Cfg").expect("signature");
        assert_eq!(sig.source, SignatureSource::Attributes);
        assert_eq!(sig.params.len(), 1);
        assert_eq!(sig.params[0].name, "address");

        // The real one, after a docstring, is still found.
        let real = "class Cfg:\n    \"\"\"Doc.\"\"\"\n\n    def __init__(self, address: str):\n        pass\n";
        let sig = init_signature(real, "Cfg").expect("signature");
        assert_eq!(sig.source, SignatureSource::Init);
        assert_eq!(sig.params[0].name, "address");
    }

    #[test]
    fn a_docstring_at_column_zero_does_not_end_the_class() {
        let src = "@dataclass\nclass Cfg:\n    \"\"\"Doc.\n\nAttributes:\n    address: the VISA address\n    \"\"\"\n\n    port: int = 1\n";
        let sig = init_signature(src, "Cfg").expect("signature");
        let names: Vec<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["port"]);
    }

    #[test]
    fn an_unterminated_docstring_says_nothing_rather_than_guessing() {
        // Mid-edit: the quote never closes, so every field is swallowed
        // and the signature is unknowable — silence, not invented keys.
        let src = "@dataclass\nclass Cfg:\n    \"\"\"Half-typed\n    address: str\n";
        assert_eq!(init_signature(src, "Cfg"), None);
    }

    #[test]
    fn attribute_signature_is_uncertain_without_a_generating_decorator_or_base() {
        let plain = "class Cfg(Base):\n    address: str\n";
        assert!(!init_signature(plain, "Cfg").unwrap().certain);
        let pyd = "class Cfg(BaseModel):\n    address: str\n";
        assert!(init_signature(pyd, "Cfg").unwrap().certain);
        let qualified = "@dataclasses.dataclass(frozen=True)\nclass Cfg:\n    address: str\n";
        assert!(init_signature(qualified, "Cfg").unwrap().certain);
        let attrs = "@attrs.define\nclass Cfg:\n    address: str\n";
        assert!(init_signature(attrs, "Cfg").unwrap().certain);
    }

    #[test]
    fn init_signature_is_none_when_unknowable() {
        // Inherits __init__ from elsewhere: nothing to read here.
        assert_eq!(init_signature("class B(Base):\n    pass\n", "B"), None);
        // Not in this file at all.
        assert_eq!(init_signature(PLUG, "Other"), None);
    }

    #[test]
    fn suggestion_tolerates_two_edits_and_case() {
        let params = vec![
            InitParam {
                name: "address".into(),
                required: true,
            },
            InitParam {
                name: "port".into(),
                required: false,
            },
        ];
        assert_eq!(suggestion_for("adress", &params), Some("address"));
        assert_eq!(suggestion_for("Address", &params), Some("address"));
        assert_eq!(suggestion_for("prot", &params), Some("port"));
        assert_eq!(suggestion_for("baudrate", &params), None);
    }

    #[test]
    fn suggestion_is_the_closest_param_not_the_first() {
        let params = vec![
            InitParam {
                name: "porta".into(),
                required: false,
            },
            InitParam {
                name: "port".into(),
                required: false,
            },
        ];
        assert_eq!(suggestion_for("prot", &params), Some("port"));
    }
}
