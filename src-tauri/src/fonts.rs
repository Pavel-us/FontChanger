//! Font and font-size rewriting for every supported format.
//!
//! Everything here is a pure function (text or bytes in, text or bytes out): no file system, no Tauri.
//! That keeps it easy to test on its own (`cargo test`). `engine.rs` takes care of files, ZIP containers,
//! encodings and parallelism.
//!
//! Semantics (the same in every format):
//!  * a font rule is `from -> to`; `from == None` means "any font";
//!  * a size rule is `from -> to` in points; `from == None` means "any size";
//!  * fonts and sizes are matched independently of each other;
//!  * rules are tried in order, the first match wins (rules with "any" are tried last);
//!  * symbol fonts (Symbol, Wingdings, ...) are left alone by "any font" rules when `keep_symbols` is on;
//!  * fonts for Asian / complex scripts are changed by "any font" rules only when `scripts` is on.

use regex::{Captures, Regex};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Compiles a regex once per call site.
macro_rules! re {
    ($pat:expr) => {{
        static R: OnceLock<Regex> = OnceLock::new();
        R.get_or_init(|| Regex::new($pat).unwrap())
    }};
}

// ───────────── rules ─────────────

#[derive(Debug, Clone)]
pub struct FontRule {
    /// lower-case source font; `None` = any font
    pub from: Option<String>,
    pub to: String,
}

#[derive(Debug, Clone)]
pub struct SizeRule {
    /// source size in points; `None` = any size
    pub from: Option<f64>,
    pub to: f64,
}

/// Where a size was found (scan only): on visible text, in a style / default / list definition, or on something
/// that has no visible text (paragraph mark, empty run). `CTX_UNKNOWN` = the format does not tell (HTML, CSS).
pub const CTX_UNKNOWN: u8 = 1;
pub const CTX_TEXT: u8 = 2;
pub const CTX_STYLE: u8 = 4;
pub const CTX_HIDDEN: u8 = 8;

#[derive(Debug, Clone, Default)]
pub struct Rules {
    pub fonts: Vec<FontRule>,
    pub sizes: Vec<SizeRule>,
    pub scripts: bool,
    pub keep_symbols: bool,
    /// "any size" rules leave sizes below this many points alone (0 = off); rules with an explicit source size always apply
    pub min_size: f64,
    /// scan mode (see `Rules::probing`): nothing is ever changed, every font and size that is looked at is recorded
    pub probe: Option<Arc<Mutex<Probe>>>,
}

/// What a scan found in one file.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// lower-case name -> (name as written, seen in a Latin/Cyrillic slot, symbol font)
    pub fonts: HashMap<String, (String, bool, bool)>,
    /// size in hundredths of a point -> OR of the CTX_* flags it was seen with
    pub sizes: HashMap<i64, u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub fonts: usize,
    pub sizes: usize,
}

impl Counts {
    pub fn total(&self) -> usize {
        self.fonts + self.sizes
    }
    pub fn add(&mut self, o: Counts) {
        self.fonts += o.fonts;
        self.sizes += o.sizes;
    }
}

/// Fonts that carry symbols or pictograms: replacing them turns bullets and icons into letters.
pub fn is_symbol_font(low: &str) -> bool {
    const NAMES: &[&str] = &[
        "symbol",
        "webdings",
        "marlett",
        "mt extra",
        "opensymbol",
        "zapfdingbats",
        "zapf dingbats",
        "ms outlook",
        "bookshelf symbol 7",
        "segoe ui symbol",
        "segoe ui emoji",
        "apple color emoji",
        "monotype sorts",
    ];
    low.starts_with("wingdings") || NAMES.contains(&low)
}

impl Rules {
    /// Rules for scanning. Every function in this module asks `font_for_ext` / `size_for` about each font and
    /// size it meets, so running them with these rules (and throwing the result away) lists what a file contains,
    /// with the very same parsing that the real change uses.
    pub fn probing() -> (Rules, Arc<Mutex<Probe>>) {
        let probe = Arc::new(Mutex::new(Probe::default()));
        let rules = Rules {
            // dummy rules that can never match: they only keep the "is there anything to do" checks switched on
            fonts: vec![FontRule { from: Some("\u{0}probe".to_string()), to: String::new() }],
            sizes: vec![SizeRule { from: Some(-1.0), to: 1.0 }],
            scripts: true,
            keep_symbols: true,
            min_size: 0.0,
            probe: Some(probe.clone()),
        };
        (rules, probe)
    }

    pub fn is_probe(&self) -> bool {
        self.probe.is_some()
    }

    /// New name for a font, or `None` if it must stay as it is.
    /// `latin`: the slot holds Latin/Cyrillic/Greek text (false for East Asian and complex-script slots).
    /// `symbol_hint`: the caller already knows that this is a symbol font (RTF charset 2).
    pub fn font_for_ext(&self, name: &str, latin: bool, symbol_hint: bool) -> Option<String> {
        let n = name.trim();
        if n.is_empty() || n.starts_with('+') {
            return None; // empty or a theme reference such as +mn-lt
        }
        let low = n.to_lowercase();
        let symbol = symbol_hint || is_symbol_font(&low);
        if let Some(p) = &self.probe {
            if let Ok(mut g) = p.lock() {
                let e = g.fonts.entry(low).or_insert_with(|| (n.to_string(), false, symbol));
                if latin {
                    e.1 = true;
                }
                if symbol {
                    e.2 = true;
                }
            }
            return None;
        }
        for rule in &self.fonts {
            let hit = match &rule.from {
                Some(f) => *f == low,
                None => !(self.keep_symbols && symbol) && (latin || self.scripts),
            };
            if hit {
                if rule.to.to_lowercase() == low {
                    return None;
                }
                return Some(rule.to.clone());
            }
        }
        None
    }

    pub fn font_for(&self, name: &str, latin: bool) -> Option<String> {
        self.font_for_ext(name, latin, false)
    }

    /// New size in points. `pt == None` means "a size that cannot be expressed in points" (em, %, keywords):
    /// only "any size" rules apply to it.
    pub fn size_for(&self, pt: Option<f64>) -> Option<f64> {
        self.size_for_ctx(pt, CTX_UNKNOWN)
    }

    /// Like `size_for`; `ctx` (a CTX_* value) only matters to a scan, which records where the size was seen.
    pub fn size_for_ctx(&self, pt: Option<f64>, ctx: u8) -> Option<f64> {
        if let Some(p) = &self.probe {
            if let Some(v) = pt {
                if let Ok(mut g) = p.lock() {
                    *g.sizes.entry((v * 100.0).round() as i64).or_insert(0) |= ctx;
                }
            }
            return None;
        }
        for rule in &self.sizes {
            let hit = match rule.from {
                Some(f) => pt.map_or(false, |p| (p - f).abs() < 0.01),
                None => pt.map_or(true, |p| p + 0.001 >= self.min_size),
            };
            if hit {
                if let Some(p) = pt {
                    if (p - rule.to).abs() < 0.01 {
                        return None;
                    }
                }
                return Some(rule.to);
            }
        }
        None
    }

    /// Target of the first "any font" rule.
    pub fn any_font(&self) -> Option<String> {
        self.fonts.iter().find(|r| r.from.is_none()).map(|r| r.to.clone())
    }
}

// ───────────── small helpers ─────────────

pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub(crate) fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        if let Some(j) = after.find(';') {
            let ent = &after[1..j];
            let rep: Option<String> = match ent {
                "amp" => Some("&".into()),
                "lt" => Some("<".into()),
                "gt" => Some(">".into()),
                "quot" => Some("\"".into()),
                "apos" => Some("'".into()),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32).map(|c| c.to_string())
                }
                _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32).map(|c| c.to_string()),
                _ => None,
            };
            if let Some(r) = rep {
                out.push_str(&r);
                rest = &after[j + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

/// 12 -> "12", 10.5 -> "10.5", 10.25 -> "10.25"
pub fn fmt_num(x: f64) -> String {
    let s = format!("{:.2}", x);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

struct AttrPos {
    /// start of the attribute name
    start: usize,
    /// value start / end (without quotes)
    vs: usize,
    ve: usize,
}

/// Finds ` name="value"` in the attribute part of a tag. Names are matched exactly (`w:sz` never matches `w:szCs`).
fn find_attr(attrs: &str, name: &str) -> Option<AttrPos> {
    let b = attrs.as_bytes();
    let mut from = 0;
    while from < attrs.len() {
        let pos = match attrs[from..].find(name) {
            Some(p) => from + p,
            None => return None,
        };
        from = pos + name.len();
        if pos > 0 && !b[pos - 1].is_ascii_whitespace() {
            continue;
        }
        let mut i = pos + name.len();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || (b[i] != b'"' && b[i] != b'\'') {
            continue;
        }
        let q = b[i] as char;
        let vs = i + 1;
        let ve = match attrs[vs..].find(q) {
            Some(p) => vs + p,
            None => return None,
        };
        return Some(AttrPos { start: pos, vs, ve });
    }
    None
}

fn attr_value(attrs: &str, name: &str) -> Option<String> {
    find_attr(attrs, name).map(|p| attrs[p.vs..p.ve].to_string())
}

/// Replaces the value of an attribute (or appends the attribute). `value` must already be escaped.
fn set_attr(attrs: &str, name: &str, value: &str) -> String {
    match find_attr(attrs, name) {
        Some(p) => format!("{}{}{}", &attrs[..p.vs], value, &attrs[p.ve..]),
        None => format!("{} {}=\"{}\"", attrs, name, value),
    }
}

fn remove_attr(attrs: &str, name: &str) -> String {
    match find_attr(attrs, name) {
        Some(p) => {
            let b = attrs.as_bytes();
            let mut s = p.start;
            while s > 0 && b[s - 1].is_ascii_whitespace() {
                s -= 1;
            }
            format!("{}{}", &attrs[..s], &attrs[p.ve + 1..])
        }
        None => attrs.to_string(),
    }
}

// ───────────── Word (docx, docm, dotx, dotm) ─────────────

/// Word stores a font in `<w:rFonts w:ascii=.. w:hAnsi=.. w:eastAsia=.. w:cs=..>` and a size in `<w:sz w:val>` (half-points).
/// `*Theme` attributes are left alone: they point to the theme fonts, which `theme_part` rewrites.
pub fn word_part(xml: &str, name: &str, r: &Rules) -> (String, Counts) {
    let mut c = Counts::default();
    let mut out = if r.fonts.is_empty() {
        xml.to_string()
    } else {
        re!(r"<w:rFonts((?:\s(?:[^>/]|/[^>])*)?)/>")
            .replace_all(xml, |caps: &Captures| {
                let (na, n) = rfonts_attrs(&caps[1], r);
                if n == 0 {
                    caps[0].to_string()
                } else {
                    c.fonts += n;
                    format!("<w:rFonts{}/>", na)
                }
            })
            .into_owned()
    };
    if !r.sizes.is_empty() {
        // a scan also wants to know WHERE each size sits (text, style, invisible); a change does not
        let zones = if r.is_probe() { Some(word_zones(&out)) } else { None };
        out = re!(r"<w:(sz|szCs)((?:\s(?:[^>/]|/[^>])*)?)/>")
            .replace_all(&out, |caps: &Captures| {
                let tag = &caps[1];
                let attrs = &caps[2];
                if r.is_probe() && tag != "sz" {
                    return caps[0].to_string(); // szCs is the size for complex scripts only
                }
                if let Some(p) = find_attr(attrs, "w:val") {
                    if let Ok(half) = attrs[p.vs..p.ve].trim().parse::<f64>() {
                        let ctx = match &zones {
                            Some(z) => z.ctx(caps.get(0).map_or(0, |m| m.start())),
                            None => CTX_UNKNOWN,
                        };
                        if let Some(new_pt) = r.size_for_ctx(Some(half / 2.0), ctx) {
                            let new_half = (new_pt * 2.0).round();
                            if (new_half - half).abs() > 0.01 {
                                c.sizes += 1;
                                let na = set_attr(attrs, "w:val", &format!("{}", new_half as i64));
                                return format!("<w:{}{}/>", tag, na);
                            }
                        }
                    }
                }
                caps[0].to_string()
            })
            .into_owned();
    }
    if name == "word/styles.xml" {
        if let Some(to) = r.any_font() {
            if let Some(new) = ensure_default_rfonts(&out, &to, r.scripts) {
                out = new;
                c.fonts += 1;
            }
        }
    }
    (out, c)
}

/// Regions of a Word part, to tell where a `<w:sz>` sits.
struct WordZones {
    /// `<w:style>`, `<w:docDefaults>`, `<w:lvl>`
    style: Vec<(usize, usize)>,
    /// `<w:pPr>`: formatting of the paragraph mark
    ppr: Vec<(usize, usize)>,
    /// runs that contain visible characters
    text_runs: Vec<(usize, usize)>,
}

fn inside(zones: &[(usize, usize)], pos: usize) -> bool {
    let i = zones.partition_point(|z| z.0 <= pos);
    i > 0 && pos < zones[i - 1].1
}

fn word_zones(xml: &str) -> WordZones {
    let style = re!(r"(?s)<w:(?:style|docDefaults|lvl)\b.*?</w:(?:style|docDefaults|lvl)>")
        .find_iter(xml)
        .map(|m| (m.start(), m.end()))
        .collect();
    let ppr = re!(r"(?s)<w:pPr>.*?</w:pPr>").find_iter(xml).map(|m| (m.start(), m.end())).collect();
    let mut text_runs = Vec::new();
    for m in re!(r"(?s)<w:r[ >].*?</w:r>").find_iter(xml) {
        if re!(r"<w:t(?:\s[^>]*)?>\s*[^<\s]").is_match(m.as_str()) {
            text_runs.push((m.start(), m.end()));
        }
    }
    WordZones { style, ppr, text_runs }
}

impl WordZones {
    fn ctx(&self, pos: usize) -> u8 {
        if inside(&self.style, pos) {
            CTX_STYLE
        } else if inside(&self.ppr, pos) {
            CTX_HIDDEN
        } else if inside(&self.text_runs, pos) {
            CTX_TEXT
        } else {
            CTX_HIDDEN
        }
    }
}

fn rfonts_attrs(attrs: &str, r: &Rules) -> (String, usize) {
    let mut cur = attrs.to_string();
    let mut n = 0;
    for (an, latin) in [("w:ascii", true), ("w:hAnsi", true), ("w:eastAsia", false), ("w:cs", false)] {
        let found = attr_value(&cur, an);
        if let Some(raw) = found {
            let val = xml_unescape(&raw);
            if let Some(to) = r.font_for(&val, latin) {
                cur = set_attr(&cur, an, &xml_escape(&to));
                n += 1;
            }
        }
    }
    (cur, n)
}

/// A document whose defaults carry no font at all uses the application default.
/// "Any font" has to cover that too, so the font is added to `<w:rPrDefault>`.
fn ensure_default_rfonts(xml: &str, to: &str, scripts: bool) -> Option<String> {
    let a = xml.find("<w:rPrDefault>")?;
    let rest = &xml[a..];
    let end_def = rest.find("</w:rPrDefault>")?;
    let seg = &rest[..end_def];
    let ro = seg.find("<w:rPr>")?;
    if seg.contains("<w:rFonts") {
        return None;
    }
    let at = a + ro + "<w:rPr>".len();
    let e = xml_escape(to);
    let mut ins = format!("<w:rFonts w:ascii=\"{}\" w:hAnsi=\"{}\"", e, e);
    if scripts {
        ins.push_str(&format!(" w:eastAsia=\"{}\" w:cs=\"{}\"", e, e));
    }
    ins.push_str("/>");
    Some(format!("{}{}{}", &xml[..at], ins, &xml[at..]))
}

// ───────────── DrawingML: PowerPoint slides, themes, text boxes in Excel ─────────────

/// `<a:latin typeface=..>`, `<a:ea ..>`, `<a:cs ..>` and `sz` (hundredths of a point) on `a:rPr`, `a:defRPr`, `a:endParaRPr`.
/// `styles_only`: the part holds styles, not content (masters, layouts, presentation.xml): a scan files all its sizes under "styles".
pub fn drawingml_part(xml: &str, r: &Rules, styles_only: bool) -> (String, Counts) {
    let mut c = Counts::default();
    let mut out = xml.to_string();
    if !r.fonts.is_empty() {
        out = replace_typefaces(&out, r, &mut c);
    }
    if !r.sizes.is_empty() {
        // runs that contain visible text (scan only)
        let mut text_runs: Vec<(usize, usize)> = Vec::new();
        if r.is_probe() && !styles_only {
            for m in re!(r"(?s)<a:(?:r|fld)\b.*?</a:(?:r|fld)>").find_iter(&out) {
                if re!(r"<a:t(?:\s[^>]*)?>\s*[^<\s]").is_match(m.as_str()) {
                    text_runs.push((m.start(), m.end()));
                }
            }
        }
        out = re!(r"<a:(rPr|defRPr|endParaRPr)((?:\s(?:[^>/]|/[^>])*)?)(/?)>")
            .replace_all(&out, |caps: &Captures| {
                let tag = &caps[1];
                let attrs = &caps[2];
                let slash = &caps[3];
                if let Some(p) = find_attr(attrs, "sz") {
                    if let Ok(hundredths) = attrs[p.vs..p.ve].trim().parse::<f64>() {
                        let ctx = if styles_only || tag == "defRPr" {
                            CTX_STYLE
                        } else if tag == "endParaRPr" {
                            CTX_HIDDEN
                        } else if inside(&text_runs, caps.get(0).map_or(0, |m| m.start())) {
                            CTX_TEXT
                        } else {
                            CTX_HIDDEN
                        };
                        if let Some(new_pt) = r.size_for_ctx(Some(hundredths / 100.0), ctx) {
                            let new_h = (new_pt * 100.0).round();
                            if (new_h - hundredths).abs() > 0.5 {
                                c.sizes += 1;
                                let na = set_attr(attrs, "sz", &format!("{}", new_h as i64));
                                return format!("<a:{}{}{}>", tag, na, slash);
                            }
                        }
                    }
                }
                caps[0].to_string()
            })
            .into_owned();
    }
    (out, c)
}

fn replace_typefaces(xml: &str, r: &Rules, c: &mut Counts) -> String {
    re!(r"<a:(latin|ea|cs)((?:\s(?:[^>/]|/[^>])*)?)/>")
        .replace_all(xml, |caps: &Captures| {
            let slot = &caps[1];
            let attrs = &caps[2];
            if let Some(raw) = attr_value(attrs, "typeface") {
                let val = xml_unescape(&raw);
                if let Some(to) = r.font_for(&val, slot == "latin") {
                    let mut na = set_attr(attrs, "typeface", &xml_escape(&to));
                    // panose / pitchFamily / charset describe the OLD font
                    for old in ["panose", "pitchFamily", "charset"] {
                        na = remove_attr(&na, old);
                    }
                    c.fonts += 1;
                    return format!("<a:{}{}/>", slot, na);
                }
            }
            caps[0].to_string()
        })
        .into_owned()
}

/// Theme fonts (`theme1.xml` in docx, xlsx, pptx): only the major/minor font sets, not the per-script list.
pub fn theme_part(xml: &str, r: &Rules) -> (String, Counts) {
    let mut c = Counts::default();
    if r.fonts.is_empty() {
        return (xml.to_string(), c);
    }
    let out = re!(r"(?s)<a:(?:majorFont|minorFont)>.*?</a:(?:majorFont|minorFont)>")
        .replace_all(xml, |caps: &Captures| replace_typefaces(&caps[0], r, &mut c))
        .into_owned();
    (out, c)
}

// ───────────── Excel (xlsx, xlsm, xltx, xltm) ─────────────

/// Cell fonts: `<font><sz val=".."/><name val=".."/><scheme val="minor"/></font>` in styles.xml.
pub fn excel_styles(xml: &str, r: &Rules) -> (String, Counts) {
    let mut c = Counts::default();
    let out = re!(r"(?s)<font>(.*?)</font>")
        .replace_all(xml, |caps: &Captures| {
            let inner = xl_font_inner(&caps[1], r, &mut c);
            format!("<font>{}</font>", inner)
        })
        .into_owned();
    (out, c)
}

/// Rich text runs: `<rPr><rFont val=".."/><sz val=".."/></rPr>` (shared strings, comments, inline strings).
pub fn excel_rich(xml: &str, r: &Rules) -> (String, Counts) {
    let mut c = Counts::default();
    let out = re!(r"(?s)<rPr>(.*?)</rPr>")
        .replace_all(xml, |caps: &Captures| {
            let inner = xl_font_inner(&caps[1], r, &mut c);
            format!("<rPr>{}</rPr>", inner)
        })
        .into_owned();
    (out, c)
}

fn xl_font_inner(inner: &str, r: &Rules, c: &mut Counts) -> String {
    let mut name_changed = false;
    let mut out = if r.fonts.is_empty() {
        inner.to_string()
    } else {
        re!(r"<(name|rFont)((?:\s(?:[^>/]|/[^>])*)?)/>")
            .replace_all(inner, |caps: &Captures| {
                let tag = &caps[1];
                let attrs = &caps[2];
                if let Some(raw) = attr_value(attrs, "val") {
                    let val = xml_unescape(&raw);
                    if let Some(to) = r.font_for(&val, true) {
                        name_changed = true;
                        c.fonts += 1;
                        return format!("<{}{}/>", tag, set_attr(attrs, "val", &xml_escape(&to)));
                    }
                }
                caps[0].to_string()
            })
            .into_owned()
    };
    if name_changed {
        // <scheme val="minor"/> would make Excel take the font from the theme again
        out = re!(r"<scheme(?:\s(?:[^>/]|/[^>])*)?/>").replace_all(&out, "").into_owned();
    }
    if !r.sizes.is_empty() {
        out = re!(r"<sz((?:\s(?:[^>/]|/[^>])*)?)/>")
            .replace_all(&out, |caps: &Captures| {
                let attrs = &caps[1];
                if let Some(p) = find_attr(attrs, "val") {
                    if let Ok(pt) = attrs[p.vs..p.ve].trim().parse::<f64>() {
                        if let Some(new) = r.size_for_ctx(Some(pt), CTX_TEXT) {
                            c.sizes += 1;
                            return format!("<sz{}/>", set_attr(attrs, "val", &fmt_num(new)));
                        }
                    }
                }
                caps[0].to_string()
            })
            .into_owned();
    }
    out
}

// ───────────── LibreOffice / OpenOffice (odt, ods, odp, odg and templates) ─────────────

fn unquote_family(s: &str) -> String {
    let t = s.trim();
    let t = t.trim_matches(|ch: char| ch == '\'' || ch == '"');
    t.trim().to_string()
}

fn quote_with(name: &str, q: char) -> String {
    let plain = name.chars().all(|ch| ch.is_alphanumeric() || ch == '-' || ch == '_');
    if plain {
        name.to_string()
    } else {
        format!("{}{}{}", q, name, q)
    }
}

fn quote_family(name: &str) -> String {
    quote_with(name, '\'')
}

fn decl_name_for(faces: &HashMap<String, String>, to: &str) -> String {
    let low = to.to_lowercase();
    for (k, v) in faces {
        if v.to_lowercase() == low {
            return k.clone();
        }
    }
    to.to_string()
}

fn family_attr_for(font_name_attr: &str) -> &'static str {
    match font_name_attr {
        "style:font-name-asian" => "style:font-family-asian",
        "style:font-name-complex" => "style:font-family-complex",
        _ => "fo:font-family",
    }
}

/// ODF keeps fonts as `style:font-name` (a reference to `<style:font-face>`) or as literal `fo:font-family`,
/// sizes as `fo:font-size="12pt"`. Works on `content.xml` and `styles.xml`.
/// `ctx`: CTX_TEXT for content.xml (automatic styles of the content), CTX_STYLE for styles.xml.
pub fn odf_part(xml: &str, r: &Rules, ctx: u8) -> (String, Counts) {
    let mut c = Counts::default();

    // style:name -> real family of every declared font
    let mut faces: HashMap<String, String> = HashMap::new();
    for caps in re!(r"<style:font-face((?:\s(?:[^>/]|/[^>])*)?)/?>").captures_iter(xml) {
        let attrs = &caps[1];
        if let (Some(n), Some(f)) = (attr_value(attrs, "style:name"), attr_value(attrs, "svg:font-family")) {
            faces.insert(xml_unescape(&n), unquote_family(&xml_unescape(&f)));
        }
    }
    let has_decls = xml.contains("<office:font-face-decls>");
    let mut needed: Vec<String> = Vec::new();

    let mut out = re!(r"<style:text-properties((?:\s(?:[^>/]|/[^>])*)?)(/?)>")
        .replace_all(xml, |caps: &Captures| {
            let mut attrs = caps[1].to_string();
            let mut changed = false;

            if !r.fonts.is_empty() {
                for (an, latin) in [("style:font-name", true), ("style:font-name-asian", false), ("style:font-name-complex", false)] {
                    let found = attr_value(&attrs, an);
                    if let Some(raw) = found {
                        let val = xml_unescape(&raw);
                        let family = match faces.get(&val) {
                            Some(f) => f.clone(),
                            None => val.clone(),
                        };
                        if let Some(to) = r.font_for(&family, latin) {
                            if has_decls {
                                let decl = decl_name_for(&faces, &to);
                                attrs = set_attr(&attrs, an, &xml_escape(&decl));
                                let declared = faces.values().any(|f| f.to_lowercase() == to.to_lowercase());
                                if !declared && !needed.contains(&to) {
                                    needed.push(to.clone());
                                }
                            } else {
                                // no <office:font-face-decls>: use a literal family instead of a reference
                                attrs = remove_attr(&attrs, an);
                                attrs = set_attr(&attrs, family_attr_for(an), &xml_escape(&quote_family(&to)));
                            }
                            c.fonts += 1;
                            changed = true;
                        }
                    }
                }
                for (an, latin) in [("fo:font-family", true), ("style:font-family-asian", false), ("style:font-family-complex", false)] {
                    let found = attr_value(&attrs, an);
                    if let Some(raw) = found {
                        let val = unquote_family(&xml_unescape(&raw));
                        if let Some(to) = r.font_for(&val, latin) {
                            attrs = set_attr(&attrs, an, &xml_escape(&quote_family(&to)));
                            c.fonts += 1;
                            changed = true;
                        }
                    }
                }
            }

            if !r.sizes.is_empty() {
                for an in ["fo:font-size", "style:font-size-asian", "style:font-size-complex"] {
                    if r.is_probe() && an != "fo:font-size" {
                        continue; // the Asian and complex sizes are normally the same number
                    }
                    let found = attr_value(&attrs, an);
                    if let Some(raw) = found {
                        let v = raw.trim().to_string();
                        if let Some(num) = v.strip_suffix("pt") {
                            if let Ok(pt) = num.trim().parse::<f64>() {
                                if let Some(new) = r.size_for_ctx(Some(pt), ctx) {
                                    attrs = set_attr(&attrs, an, &format!("{}pt", fmt_num(new)));
                                    c.sizes += 1;
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }

            if changed {
                format!("<style:text-properties{}{}>", attrs, &caps[2])
            } else {
                caps[0].to_string()
            }
        })
        .into_owned();

    if has_decls && !needed.is_empty() {
        let mut ins = String::new();
        for n in &needed {
            ins.push_str(&format!(
                "<style:font-face style:name=\"{}\" svg:font-family=\"{}\" style:font-family-generic=\"system\" style:font-pitch=\"variable\"/>",
                xml_escape(n),
                xml_escape(&quote_family(n))
            ));
        }
        out = out.replacen("<office:font-face-decls>", &format!("<office:font-face-decls>{}", ins), 1);
    }
    (out, c)
}

// ───────────── RTF ─────────────

/// Index of the `}` that closes the group we are currently inside (`from` points just after the opening `{`).
fn rtf_close(data: &[u8], from: usize) -> Option<usize> {
    let mut depth = 1i32;
    let mut i = from;
    while i < data.len() {
        match data[i] {
            b'\\' => {
                i += 2;
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn rtf_encode_name(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for ch in s.chars() {
        let u = ch as u32;
        if (0x20..0x7f).contains(&u) && ch != '\\' && ch != '{' && ch != '}' && ch != ';' {
            out.push(u as u8);
        } else {
            let mut buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut buf).iter() {
                out.extend_from_slice(format!("\\u{}?", *unit as i16).as_bytes());
            }
        }
    }
    out
}

/// One entry of the font table, without the surrounding braces: `\f0\fswiss\fcharset0 Arial;`
fn rtf_font_entry(entry: &[u8], r: &Rules) -> Option<Vec<u8>> {
    let n = entry.len();
    let mut i = 0;
    let mut name_start: Option<usize> = None;
    let mut name_end: Option<usize> = None;
    let mut charset: i64 = -1;
    while i < n {
        match entry[i] {
            b'{' => {
                if name_start.is_some() && name_end.is_none() {
                    name_end = Some(i);
                }
                let close = rtf_close(entry, i + 1)?;
                i = close + 1;
            }
            b'\\' => {
                if name_start.is_some() {
                    return None; // escapes inside the name: leave such entries alone
                }
                if i + 1 >= n {
                    break;
                }
                if !entry[i + 1].is_ascii_alphabetic() {
                    if entry[i + 1] == b'\'' {
                        return None;
                    }
                    i += 2;
                    continue;
                }
                let mut j = i + 1;
                while j < n && entry[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let word = &entry[i + 1..j];
                if word == b"u" {
                    return None;
                }
                let num_start = j;
                if j + 1 < n && entry[j] == b'-' && entry[j + 1].is_ascii_digit() {
                    j += 1;
                }
                while j < n && entry[j].is_ascii_digit() {
                    j += 1;
                }
                if word == b"fcharset" {
                    if let Ok(s) = std::str::from_utf8(&entry[num_start..j]) {
                        charset = s.parse::<i64>().unwrap_or(-1);
                    }
                }
                if j < n && entry[j] == b' ' {
                    j += 1; // delimiter of the control word
                }
                i = j;
            }
            b';' => {
                if name_start.is_some() && name_end.is_none() {
                    name_end = Some(i);
                }
                break;
            }
            ch => {
                if name_start.is_none() && !ch.is_ascii_whitespace() {
                    name_start = Some(i);
                }
                i += 1;
            }
        }
    }
    let ns = name_start?;
    let ne = name_end?;
    if ne <= ns {
        return None;
    }
    let name = std::str::from_utf8(&entry[ns..ne]).ok()?.trim_end();
    if name.is_empty() {
        return None;
    }
    // Hebrew, Arabic, Thai, CJK charsets are "other scripts"
    let latin = !matches!(charset, 128 | 129 | 130 | 134 | 136 | 177 | 178 | 222);
    let to = r.font_for_ext(name, latin, charset == 2)?;
    let mut res = Vec::with_capacity(entry.len() + to.len());
    res.extend_from_slice(&entry[..ns]);
    res.extend_from_slice(&rtf_encode_name(&to));
    res.extend_from_slice(&entry[ne..]);
    Some(res)
}

/// Contents of the `{\fonttbl ...}` group, starting right after the `\fonttbl` control word.
fn rtf_fonttbl(table: &[u8], r: &Rules, c: &mut Counts) -> Vec<u8> {
    let mut out = Vec::with_capacity(table.len() + 32);
    let mut i = 0;
    while i < table.len() {
        if table[i] == b'{' {
            if let Some(close) = rtf_close(table, i + 1) {
                let entry = &table[i + 1..close];
                out.push(b'{');
                match rtf_font_entry(entry, r) {
                    Some(new) => {
                        out.extend_from_slice(&new);
                        c.fonts += 1;
                    }
                    None => out.extend_from_slice(entry),
                }
                out.push(b'}');
                i = close + 1;
                continue;
            }
        }
        if table[i] == b'\\' && i + 1 < table.len() {
            out.extend_from_slice(&table[i..i + 2]);
            i += 2;
            continue;
        }
        out.push(table[i]);
        i += 1;
    }
    out
}

/// RTF: font names in the font table and `\fsN` (half-points) everywhere. Text bytes are never touched.
pub fn rtf_part(data: &[u8], r: &Rules) -> (Vec<u8>, Counts) {
    let mut c = Counts::default();
    let n = data.len();
    let mut out: Vec<u8> = Vec::with_capacity(n + 64);
    let mut i = 0;
    let mut style_end = 0usize; // end of the {\stylesheet ...} group (sizes inside it are style sizes)
    while i < n {
        let b = data[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        if i + 1 >= n {
            out.push(b);
            i += 1;
            continue;
        }
        let nb = data[i + 1];
        if nb == b'\'' {
            let end = (i + 4).min(n);
            out.extend_from_slice(&data[i..end]);
            i = end;
            continue;
        }
        if !nb.is_ascii_alphabetic() {
            out.extend_from_slice(&data[i..i + 2]);
            i += 2;
            continue;
        }
        let mut j = i + 1;
        while j < n && data[j].is_ascii_alphabetic() {
            j += 1;
        }
        let word_end = j;
        let num_start = j;
        if j + 1 < n && data[j] == b'-' && data[j + 1].is_ascii_digit() {
            j += 1;
        }
        while j < n && data[j].is_ascii_digit() {
            j += 1;
        }
        let word = &data[i + 1..word_end];
        let has_num = j > num_start && data[j - 1].is_ascii_digit();

        if word == b"fonttbl" {
            out.extend_from_slice(&data[i..j]);
            if let Some(close) = rtf_close(data, j) {
                let table = rtf_fonttbl(&data[j..close], r, &mut c);
                out.extend_from_slice(&table);
                i = close; // the closing brace is copied by the next iteration
                continue;
            }
            i = j;
            continue;
        }
        if word == b"stylesheet" {
            if let Some(close) = rtf_close(data, j) {
                style_end = close;
            }
        }
        if word == b"fs" && has_num && !r.sizes.is_empty() {
            let parsed = std::str::from_utf8(&data[num_start..j]).unwrap_or("").parse::<i64>();
            if let Ok(half) = parsed {
                let ctx = if i < style_end { CTX_STYLE } else { CTX_TEXT };
                if let Some(new) = r.size_for_ctx(Some(half as f64 / 2.0), ctx) {
                    let nh = (new * 2.0).round() as i64;
                    if nh != half {
                        out.extend_from_slice(format!("\\fs{}", nh).as_bytes());
                        c.sizes += 1;
                        i = j;
                        continue;
                    }
                }
            }
        }
        out.extend_from_slice(&data[i..j]);
        i = j;
    }
    (out, c)
}

// ───────────── HTML and CSS ─────────────

struct FamItem {
    name: String,
    quote: Option<char>,
}

fn is_generic_family(low: &str) -> bool {
    matches!(
        low,
        "serif"
            | "sans-serif"
            | "monospace"
            | "cursive"
            | "fantasy"
            | "system-ui"
            | "ui-serif"
            | "ui-sans-serif"
            | "ui-monospace"
            | "ui-rounded"
            | "emoji"
            | "math"
            | "fangsong"
            | "inherit"
            | "initial"
            | "unset"
            | "revert"
            | "revert-layer"
            | "default"
    )
}

/// Parses `Arial, "Times New Roman", sans-serif` at the start of `s`.
/// Stops at `;`, `}`, `!` or at a quote that closes an HTML attribute. Returns the items and the byte length used.
fn parse_family_list(s: &str) -> (Vec<FamItem>, usize) {
    let b = s.as_bytes();
    let mut items: Vec<FamItem> = Vec::new();
    let mut i = 0;
    let mut end = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let ch = b[i];
        if ch == b'"' || ch == b'\'' {
            match s[i + 1..].find(ch as char) {
                Some(p) => {
                    items.push(FamItem { name: s[i + 1..i + 1 + p].to_string(), quote: Some(ch as char) });
                    i = i + 1 + p + 1;
                    end = i;
                }
                None => break,
            }
        } else {
            let start = i;
            while i < b.len() && !matches!(b[i], b',' | b';' | b'}' | b'{' | b'<' | b'>' | b'"' | b'\'' | b'!') {
                i += 1;
            }
            let raw = s[start..i].trim_end();
            if raw.is_empty() {
                break;
            }
            items.push(FamItem { name: raw.to_string(), quote: None });
            end = start + raw.len();
        }
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b',' {
            i += 1;
            continue;
        }
        break;
    }
    (items, end)
}

fn serialize_item(it: &FamItem) -> String {
    match it.quote {
        Some(q) => format!("{}{}{}", q, it.name, q),
        None => it.name.clone(),
    }
}

/// New font list, or `None` if nothing changes. `q`: quote character for names that need quotes
/// (`None` = never quote, as in the HTML `face` attribute).
fn rewrite_family(items: &[FamItem], r: &Rules, q: Option<char>) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut changed = false;
    for it in items {
        let low = it.name.trim().to_lowercase();
        if is_generic_family(&low) || it.name.contains('(') {
            out.push(serialize_item(it));
            continue;
        }
        match r.font_for(&it.name, true) {
            Some(to) => {
                changed = true;
                let tl = to.to_lowercase();
                if !seen.contains(&tl) {
                    seen.push(tl);
                    out.push(match q {
                        Some(ch) => quote_with(&to, ch),
                        None => to,
                    });
                }
            }
            None => {
                seen.push(low);
                out.push(serialize_item(it));
            }
        }
    }
    if changed {
        Some(out.join(", "))
    } else {
        None
    }
}

/// `font-family` inside `@font-face` names a web font; renaming it would break the page.
fn in_font_face(text: &str, faces: &[usize], pos: usize) -> bool {
    match faces.iter().rev().find(|&&a| a < pos) {
        Some(&a) => !text[a..pos].contains('}'),
        None => false,
    }
}

fn css_families(text: &str, r: &Rules, c: &mut Counts) -> String {
    let faces: Vec<usize> = text.match_indices("@font-face").map(|(i, _)| i).collect();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in re!(r"(?i)font-family\s*:\s*").find_iter(text) {
        if m.start() < last {
            continue;
        }
        let vstart = m.end();
        let (items, consumed) = parse_family_list(&text[vstart..]);
        if items.is_empty() || in_font_face(text, &faces, m.start()) {
            continue;
        }
        // never put a quote of the same kind as the attribute that encloses the declaration
        let q = if text[vstart + consumed..].starts_with('\'') { '"' } else { '\'' };
        if let Some(new) = rewrite_family(&items, r, Some(q)) {
            out.push_str(&text[last..vstart]);
            out.push_str(&new);
            last = vstart + consumed;
            c.fonts += 1;
        }
    }
    out.push_str(&text[last..]);
    out
}

enum CssSize {
    /// absolute size in points
    Abs(f64),
    /// em, rem, %, keywords: cannot be expressed in points
    Rel,
}

fn parse_css_size(tok: &str) -> Option<CssSize> {
    let t = tok.trim().to_lowercase();
    match t.as_str() {
        "xx-small" | "x-small" | "small" | "medium" | "large" | "x-large" | "xx-large" | "xxx-large" | "smaller" | "larger" => {
            return Some(CssSize::Rel);
        }
        _ => {}
    }
    let idx = t.find(|ch: char| !(ch.is_ascii_digit() || ch == '.')).unwrap_or(t.len());
    if idx == 0 || idx == t.len() {
        return None;
    }
    let num: f64 = t[..idx].parse().ok()?;
    match &t[idx..] {
        "pt" => Some(CssSize::Abs(num)),
        "px" => Some(CssSize::Abs(num * 0.75)),
        "pc" => Some(CssSize::Abs(num * 12.0)),
        "in" => Some(CssSize::Abs(num * 72.0)),
        "cm" => Some(CssSize::Abs(num * 28.346_456_7)),
        "mm" => Some(CssSize::Abs(num * 2.834_645_67)),
        "em" | "rem" | "%" | "ex" | "ch" | "vw" | "vh" | "vmin" | "vmax" => Some(CssSize::Rel),
        _ => None,
    }
}

fn css_new_size(tok: &str, r: &Rules) -> Option<String> {
    let new = match parse_css_size(tok)? {
        CssSize::Abs(pt) => r.size_for(Some(pt)),
        CssSize::Rel => r.size_for(None),
    }?;
    Some(format!("{}pt", fmt_num(new)))
}

fn css_sizes(text: &str, r: &Rules, c: &mut Counts) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in re!(r"(?i)(?:^|[^-\w])font-size\s*:\s*").find_iter(text) {
        if m.start() < last {
            continue;
        }
        let vstart = m.end();
        let rest = &text[vstart..];
        let end = rest
            .find(|ch: char| ch.is_whitespace() || matches!(ch, ';' | '}' | '"' | '\'' | '!' | '<' | '>'))
            .unwrap_or(rest.len());
        let tok = &rest[..end];
        if tok.is_empty() {
            continue;
        }
        if let Some(new) = css_new_size(tok, r) {
            out.push_str(&text[last..vstart]);
            out.push_str(&new);
            last = vstart + end;
            c.sizes += 1;
        }
    }
    out.push_str(&text[last..]);
    out
}

struct Short {
    size_start: usize,
    main_end: usize,
    size_end: usize,
    fam_start: usize,
    fam_end: usize,
    items: Vec<FamItem>,
}

fn is_font_prefix_token(low: &str) -> bool {
    matches!(
        low,
        "normal"
            | "italic"
            | "oblique"
            | "small-caps"
            | "bold"
            | "bolder"
            | "lighter"
            | "ultra-condensed"
            | "extra-condensed"
            | "condensed"
            | "semi-condensed"
            | "semi-expanded"
            | "expanded"
            | "extra-expanded"
            | "ultra-expanded"
    ) || (low.len() == 3 && low.chars().all(|ch| ch.is_ascii_digit()))
}

/// `font: italic bold 12px/1.5 "Open Sans", Arial, sans-serif`
fn parse_shorthand(s: &str) -> Option<Short> {
    let b = s.as_bytes();
    let mut i = 0;
    for _ in 0..8 {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b';' | b'}' | b'"' | b'\'' | b'<' | b'>' | b'{') {
            i += 1;
        }
        if start == i {
            return None;
        }
        let tok = &s[start..i];
        let main = tok.split('/').next().unwrap_or(tok);
        if parse_css_size(main).is_some() {
            let main_end = start + main.len();
            let size_end = start + tok.len();
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let (items, consumed) = parse_family_list(&s[i..]);
            if items.is_empty() {
                return None;
            }
            return Some(Short { size_start: start, main_end, size_end, fam_start: i, fam_end: i + consumed, items });
        }
        if !is_font_prefix_token(&tok.to_lowercase()) {
            return None;
        }
    }
    None
}

fn css_shorthand(text: &str, r: &Rules, c: &mut Counts) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in re!(r"(?i)(?:^|[^-\w])font\s*:\s*").find_iter(text) {
        if m.start() < last {
            continue;
        }
        let vstart = m.end();
        let sh = match parse_shorthand(&text[vstart..]) {
            Some(s) => s,
            None => continue,
        };
        let main = &text[vstart + sh.size_start..vstart + sh.main_end];
        let new_size = if r.sizes.is_empty() { None } else { css_new_size(main, r) };
        let q = if text[vstart + sh.fam_end..].starts_with('\'') { '"' } else { '\'' };
        let new_family = if r.fonts.is_empty() { None } else { rewrite_family(&sh.items, r, Some(q)) };
        if new_size.is_none() && new_family.is_none() {
            continue;
        }
        out.push_str(&text[last..vstart + sh.size_start]);
        match &new_size {
            Some(n) => {
                out.push_str(n);
                c.sizes += 1;
            }
            None => out.push_str(main),
        }
        out.push_str(&text[vstart + sh.main_end..vstart + sh.size_end]); // "/line-height"
        out.push_str(&text[vstart + sh.size_end..vstart + sh.fam_start]);
        match &new_family {
            Some(f) => {
                out.push_str(f);
                c.fonts += 1;
            }
            None => out.push_str(&text[vstart + sh.fam_start..vstart + sh.fam_end]),
        }
        last = vstart + sh.fam_end;
    }
    out.push_str(&text[last..]);
    out
}

/// `<font face="Arial, Helvetica">`
fn html_font_face(text: &str, r: &Rules, c: &mut Counts) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for caps in re!(r#"(?i)<font\b[^>]*?\sface\s*=\s*(?:"([^"]*)"|'([^']*)')"#).captures_iter(text) {
        let g = match caps.get(1).or_else(|| caps.get(2)) {
            Some(g) => g,
            None => continue,
        };
        let items: Vec<FamItem> = g
            .as_str()
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| FamItem { name: s.to_string(), quote: None })
            .collect();
        if let Some(new) = rewrite_family(&items, r, None) {
            out.push_str(&text[last..g.start()]);
            out.push_str(&new);
            last = g.end();
            c.fonts += 1;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// HTML and CSS: `font-family`, `font-size`, the `font` shorthand and `<font face>`.
/// Sizes are written in points; `@font-face` declarations are never renamed.
pub fn web_text(text: &str, r: &Rules) -> (String, Counts) {
    let mut c = Counts::default();
    let mut s = css_shorthand(text, r, &mut c);
    if !r.fonts.is_empty() {
        s = css_families(&s, r, &mut c);
        s = html_font_face(&s, r, &mut c);
    }
    if !r.sizes.is_empty() {
        s = css_sizes(&s, r, &mut c);
    }
    (s, c)
}

// ───────────── tests ─────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(fonts: &[(Option<&str>, &str)], sizes: &[(Option<f64>, f64)]) -> Rules {
        let mut r = Rules { scripts: false, keep_symbols: true, ..Default::default() };
        for (f, t) in fonts {
            r.fonts.push(FontRule { from: f.map(|s| s.to_lowercase()), to: t.to_string() });
        }
        for (f, t) in sizes {
            r.sizes.push(SizeRule { from: *f, to: *t });
        }
        r.fonts.sort_by_key(|x| x.from.is_none());
        r.sizes.sort_by_key(|x| x.from.is_none());
        r
    }

    #[test]
    fn chart_part_fonts_and_sizes() {
        // chart and SmartArt parts use the same DrawingML text properties as slides
        let r = rules(&[(Some("Calibri"), "Times New Roman")], &[(None, 12.0)]);
        let xml = r#"<c:chartSpace><c:txPr><a:p><a:pPr><a:defRPr sz="1000"><a:latin typeface="Calibri"/></a:defRPr></a:pPr></a:p></c:txPr></c:chartSpace>"#;
        let (out, c) = drawingml_part(xml, &r, false);
        assert!(out.contains(r#"typeface="Times New Roman"#));
        assert!(out.contains(r#"sz="1200""#));
        assert_eq!(c.fonts, 1);
        assert_eq!(c.sizes, 1);
    }

    #[test]
    fn number_format() {
        assert_eq!(fmt_num(12.0), "12");
        assert_eq!(fmt_num(10.5), "10.5");
        assert_eq!(fmt_num(10.25), "10.25");
        assert_eq!(fmt_num(100.0), "100");
    }

    #[test]
    fn attr_helpers_match_exact_names() {
        let a = r#" w:val="24" w:valx="1""#;
        assert_eq!(attr_value(a, "w:val").unwrap(), "24");
        assert_eq!(set_attr(a, "w:val", "28"), r#" w:val="28" w:valx="1""#);
        assert_eq!(remove_attr(a, "w:valx"), r#" w:val="24""#);
        assert!(attr_value(" w:szCs=\"1\"", "w:sz").is_none());
    }

    #[test]
    fn font_rules_order_and_symbols() {
        let r = rules(&[(None, "Verdana"), (Some("Arial"), "Calibri")], &[]);
        assert_eq!(r.font_for("Arial", true).unwrap(), "Calibri"); // specific rule first
        assert_eq!(r.font_for("Times New Roman", true).unwrap(), "Verdana");
        assert!(r.font_for("Wingdings", true).is_none()); // symbol font protected
        assert!(r.font_for("SimSun", false).is_none()); // other scripts need the switch
        assert!(r.font_for("Verdana", true).is_none()); // already the target
        assert!(r.font_for("+mn-lt", true).is_none()); // theme reference
    }

    #[test]
    fn word_any_font_keeps_theme_symbols_and_other_scripts() {
        let r = rules(&[(None, "Arial")], &[]);
        let xml = r#"<w:r><w:rPr><w:rFonts w:ascii="Times New Roman" w:hAnsi="Times New Roman" w:eastAsia="SimSun" w:cs="Times New Roman"/></w:rPr></w:r><w:r><w:rPr><w:rFonts w:ascii="Wingdings" w:hAnsi="Wingdings"/></w:rPr></w:r><w:r><w:rPr><w:rFonts w:asciiTheme="minorHAnsi"/></w:rPr></w:r>"#;
        let (out, c) = word_part(xml, "word/document.xml", &r);
        assert!(out.contains(r#"<w:rFonts w:ascii="Arial" w:hAnsi="Arial" w:eastAsia="SimSun" w:cs="Times New Roman"/>"#));
        assert!(out.contains(r#"<w:rFonts w:ascii="Wingdings" w:hAnsi="Wingdings"/>"#));
        assert!(out.contains(r#"<w:rFonts w:asciiTheme="minorHAnsi"/>"#));
        assert_eq!(c, Counts { fonts: 2, sizes: 0 });
    }

    #[test]
    fn word_scripts_switch_changes_all_slots() {
        let mut r = rules(&[(None, "Arial")], &[]);
        r.scripts = true;
        let xml = r#"<w:rFonts w:ascii="Calibri" w:eastAsia="SimSun" w:cs="Tahoma"/>"#;
        let (out, c) = word_part(xml, "word/document.xml", &r);
        assert_eq!(out, r#"<w:rFonts w:ascii="Arial" w:eastAsia="Arial" w:cs="Arial"/>"#);
        assert_eq!(c.fonts, 3);
    }

    #[test]
    fn word_explicit_font_only_matches_that_font() {
        let r = rules(&[(Some("arial"), "Cambria")], &[]);
        let xml = r#"<w:rFonts w:ascii="Arial" w:hAnsi="Arial"/><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri"/>"#;
        let (out, c) = word_part(xml, "word/document.xml", &r);
        assert_eq!(out, r#"<w:rFonts w:ascii="Cambria" w:hAnsi="Cambria"/><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri"/>"#);
        assert_eq!(c.fonts, 2);
    }

    #[test]
    fn word_sizes_are_half_points() {
        let r = rules(&[], &[(Some(12.0), 14.0)]);
        let xml = r#"<w:sz w:val="24"/><w:szCs w:val="24"/><w:sz w:val="28"/><w:top w:sz="24"/>"#;
        let (out, c) = word_part(xml, "word/document.xml", &r);
        assert_eq!(out, r#"<w:sz w:val="28"/><w:szCs w:val="28"/><w:sz w:val="28"/><w:top w:sz="24"/>"#);
        assert_eq!(c.sizes, 2);
    }

    #[test]
    fn word_fractional_size_and_any_size() {
        let r = rules(&[], &[(None, 10.5)]);
        let (out, c) = word_part(r#"<w:sz w:val="24"/><w:sz w:val="21"/>"#, "word/document.xml", &r);
        assert_eq!(out, r#"<w:sz w:val="21"/><w:sz w:val="21"/>"#);
        assert_eq!(c.sizes, 1);
    }

    #[test]
    fn word_defaults_get_a_font_when_missing() {
        let r = rules(&[(None, "Arial")], &[]);
        let xml = "<w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val=\"22\"/></w:rPr></w:rPrDefault></w:docDefaults>";
        let (out, c) = word_part(xml, "word/styles.xml", &r);
        assert!(out.contains(r#"<w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/><w:sz w:val="22"/>"#));
        assert_eq!(c.fonts, 1);
        let (_, c2) = word_part(xml, "word/document.xml", &r); // only styles.xml gets this
        assert_eq!(c2.fonts, 0);
    }

    #[test]
    fn theme_fonts_are_rewritten_but_script_list_is_not() {
        let r = rules(&[(None, "Arial")], &[]);
        let xml = r#"<a:majorFont><a:latin typeface="Calibri Light" panose="020F0302020204030204"/><a:ea typeface=""/><a:cs typeface=""/><a:font script="Jpan" typeface="Yu Gothic"/></a:majorFont><a:minorFont><a:latin typeface="Calibri" panose="020F0502020204030204"/></a:minorFont>"#;
        let (out, c) = theme_part(xml, &r);
        assert!(out.contains(r#"<a:latin typeface="Arial"/>"#));
        assert!(out.contains(r#"<a:font script="Jpan" typeface="Yu Gothic"/>"#));
        assert!(!out.contains("panose"));
        assert_eq!(c.fonts, 2);
    }

    #[test]
    fn pptx_fonts_and_sizes() {
        let r = rules(&[(Some("calibri"), "Georgia")], &[(Some(18.0), 20.0)]);
        let xml = r#"<a:rPr lang="en-US" sz="1800" dirty="0"><a:latin typeface="Calibri" panose="x" pitchFamily="34" charset="0"/></a:rPr><a:endParaRPr sz="1800"/><a:defRPr sz="3200"/><a:latin typeface="+mn-lt"/>"#;
        let (out, c) = drawingml_part(xml, &r, false);
        assert!(out.contains(r#"<a:rPr lang="en-US" sz="2000" dirty="0">"#));
        assert!(out.contains(r#"<a:latin typeface="Georgia"/>"#));
        assert!(out.contains(r#"<a:endParaRPr sz="2000"/>"#));
        assert!(out.contains(r#"<a:defRPr sz="3200"/>"#));
        assert!(out.contains(r#"<a:latin typeface="+mn-lt"/>"#));
        assert_eq!(c, Counts { fonts: 1, sizes: 2 });
    }

    #[test]
    fn excel_styles_drop_scheme_when_name_changes() {
        let r = rules(&[(None, "Arial")], &[(None, 10.0)]);
        let xml = r#"<fonts count="1"><font><sz val="11"/><color theme="1"/><name val="Calibri"/><family val="2"/><scheme val="minor"/></font></fonts>"#;
        let (out, c) = excel_styles(xml, &r);
        assert!(out.contains(r#"<sz val="10"/>"#));
        assert!(out.contains(r#"<name val="Arial"/>"#));
        assert!(!out.contains("scheme"));
        assert_eq!(c, Counts { fonts: 1, sizes: 1 });
    }

    #[test]
    fn excel_rich_text_runs() {
        let r = rules(&[(Some("tahoma"), "Verdana")], &[]);
        let xml = r#"<si><r><rPr><b/><sz val="9"/><rFont val="Tahoma"/><family val="2"/></rPr><t>x</t></r></si>"#;
        let (out, c) = excel_rich(xml, &r);
        assert!(out.contains(r#"<rFont val="Verdana"/>"#));
        assert_eq!(c.fonts, 1);
    }

    #[test]
    fn odf_font_reference_and_declaration() {
        let r = rules(&[(None, "Arial")], &[(Some(12.0), 14.0)]);
        let xml = r#"<office:document-content><office:font-face-decls><style:font-face style:name="Liberation Serif" svg:font-family="&apos;Liberation Serif&apos;"/></office:font-face-decls><office:automatic-styles><style:style style:name="P1"><style:text-properties style:font-name="Liberation Serif" fo:font-size="12pt" style:font-size-asian="12pt"/></style:style></office:automatic-styles></office:document-content>"#;
        let (out, c) = odf_part(xml, &r, CTX_TEXT);
        assert!(out.contains(r#"style:font-name="Arial""#));
        assert!(out.contains(r#"<style:font-face style:name="Arial" svg:font-family="Arial""#));
        assert!(out.contains(r#"fo:font-size="14pt""#));
        assert!(out.contains(r#"style:font-size-asian="14pt""#));
        assert_eq!(c, Counts { fonts: 1, sizes: 2 });
    }

    #[test]
    fn odf_explicit_font_resolved_through_declaration() {
        let r = rules(&[(Some("liberation serif"), "Georgia")], &[]);
        let xml = r#"<office:font-face-decls><style:font-face style:name="Liberation Serif1" svg:font-family="&apos;Liberation Serif&apos;"/></office:font-face-decls><style:text-properties style:font-name="Liberation Serif1"/>"#;
        let (out, c) = odf_part(xml, &r, CTX_TEXT);
        assert!(out.contains(r#"<style:text-properties style:font-name="Georgia"/>"#));
        assert_eq!(c.fonts, 1);
    }

    #[test]
    fn odf_literal_family_attribute() {
        let r = rules(&[(Some("arial"), "Times New Roman")], &[]);
        let xml = r#"<style:text-properties fo:font-family="Arial"/>"#;
        let (out, _) = odf_part(xml, &r, CTX_TEXT);
        assert_eq!(out, r#"<style:text-properties fo:font-family="'Times New Roman'"/>"#);
    }

    fn rtf_text(data: &[u8]) -> String {
        String::from_utf8_lossy(data).to_string()
    }

    #[test]
    fn rtf_font_table_and_sizes() {
        let r = rules(&[(None, "Calibri")], &[(Some(12.0), 14.0)]);
        let src = br"{\rtf1\ansi\deff0{\fonttbl{\f0\fswiss\fcharset204 Arial;}{\f1\fnil\fcharset2 Symbol;}}\f0\fs24 Hello \fs20 small\par}";
        let (out, c) = rtf_part(src, &r);
        let s = rtf_text(&out);
        assert!(s.contains(r"{\f0\fswiss\fcharset204 Calibri;}"));
        assert!(s.contains(r"{\f1\fnil\fcharset2 Symbol;}"));
        assert!(s.contains(r"\fs28 Hello \fs20 small"));
        assert_eq!(c, Counts { fonts: 1, sizes: 1 });
    }

    #[test]
    fn rtf_keeps_text_bytes_and_escaped_backslashes() {
        let r = rules(&[], &[(None, 11.0)]);
        let src = br"{\rtf1{\fonttbl{\f0 Arial;}}\f0\fs24 a\\fs24 b \'e0\'e1 \fs30 c}";
        let (out, c) = rtf_part(src, &r);
        let s = rtf_text(&out);
        assert!(s.contains(r"\fs22 a\\fs24 b \'e0\'e1 \fs22 c"));
        assert_eq!(c.sizes, 2);
    }

    #[test]
    fn rtf_font_with_alt_name_group() {
        let r = rules(&[(Some("arial"), "Verdana")], &[]);
        let src = br"{\rtf1{\fonttbl{\f3\fnil\fcharset0 Arial{\*\falt Helvetica};}}}";
        let (out, c) = rtf_part(src, &r);
        assert!(rtf_text(&out).contains(r"{\f3\fnil\fcharset0 Verdana{\*\falt Helvetica};}"));
        assert_eq!(c.fonts, 1);
    }

    #[test]
    fn css_font_family_lists() {
        let r = rules(&[(None, "Georgia")], &[]);
        let css = r#"body { font-family: Arial, Helvetica, sans-serif; } h1 { font-family: "Open Sans", serif; } code { font-family: Consolas, monospace }"#;
        let (out, c) = web_text(css, &r);
        assert!(out.contains("font-family: Georgia, sans-serif;"));
        assert!(out.contains("font-family: Georgia, serif;"));
        assert!(out.contains("font-family: Georgia, monospace }"));
        assert_eq!(c.fonts, 3);
    }

    #[test]
    fn css_explicit_font_in_place_and_inline_style() {
        let r = rules(&[(Some("arial"), "Times New Roman")], &[]);
        let html = r#"<p style="font-family: Arial, sans-serif" class="x">a</p><p style='font-family:"Arial"'>b</p>"#;
        let (out, c) = web_text(html, &r);
        assert!(out.contains(r#"<p style="font-family: 'Times New Roman', sans-serif" class="x">"#));
        assert!(out.contains(r#"<p style='font-family:"Times New Roman"'>"#));
        assert_eq!(c.fonts, 2);
    }

    #[test]
    fn css_font_face_blocks_are_left_alone() {
        let r = rules(&[(None, "Georgia")], &[]);
        let css = r#"@font-face { font-family: "MyWeb"; src: url(a.woff); } p { font-family: Arial; }"#;
        let (out, c) = web_text(css, &r);
        assert!(out.contains(r#"@font-face { font-family: "MyWeb"; src: url(a.woff); }"#));
        assert!(out.contains("p { font-family: Georgia; }"));
        assert_eq!(c.fonts, 1);
    }

    #[test]
    fn css_sizes_in_any_unit() {
        let r = rules(&[], &[(Some(12.0), 14.0)]);
        let css = "a { font-size: 16px; } b { font-size: 12pt; } i { font-size: 1.2em; } u { font-size: 12pt!important; }";
        let (out, c) = web_text(css, &r);
        // 16px = 12pt -> 14pt
        assert!(out.contains("a { font-size: 14pt; }"));
        assert!(out.contains("b { font-size: 14pt; }"));
        assert!(out.contains("i { font-size: 1.2em; }"));
        assert!(out.contains("u { font-size: 14pt!important; }"));
        assert_eq!(c.sizes, 3);
    }

    #[test]
    fn css_any_size_also_flattens_relative_sizes() {
        let r = rules(&[], &[(None, 12.0)]);
        let (out, c) = web_text("h1 { font-size: 2em } p { font-size: large }", &r);
        assert_eq!(out, "h1 { font-size: 12pt } p { font-size: 12pt }");
        assert_eq!(c.sizes, 2);
    }

    #[test]
    fn css_shorthand() {
        let r = rules(&[(None, "Georgia")], &[(None, 11.0)]);
        let css = "p { font: italic bold 12px/1.5 \"Open Sans\", Arial, sans-serif; } q { font: inherit; }";
        let (out, c) = web_text(css, &r);
        assert!(out.contains("p { font: italic bold 11pt/1.5 Georgia, sans-serif; }"));
        assert!(out.contains("q { font: inherit; }"));
        assert_eq!(c, Counts { fonts: 1, sizes: 1 });
    }

    #[test]
    fn html_font_face_attribute() {
        let r = rules(&[(Some("arial"), "Verdana")], &[]);
        let (out, c) = web_text(r#"<font color="red" face="Arial, Helvetica">x</font>"#, &r);
        assert_eq!(out, r#"<font color="red" face="Verdana, Helvetica">x</font>"#);
        assert_eq!(c.fonts, 1);
    }

    #[test]
    fn probe_lists_fonts_and_sizes_without_changing_anything() {
        let (r, probe) = Rules::probing();
        let xml = r#"<w:rFonts w:ascii="Arial" w:hAnsi="Arial" w:eastAsia="SimSun"/><w:rFonts w:ascii="Wingdings"/><w:rFonts w:asciiTheme="minorHAnsi"/><w:sz w:val="24"/><w:szCs w:val="21"/>"#;
        let (out, c) = word_part(xml, "word/document.xml", &r);
        assert_eq!(out, xml);
        assert_eq!(c.total(), 0);
        let p = probe.lock().unwrap();
        assert_eq!(p.fonts.len(), 3);
        assert_eq!(p.fonts["arial"], ("Arial".to_string(), true, false));
        assert_eq!(p.fonts["simsun"], ("SimSun".to_string(), false, false));
        assert_eq!(p.fonts["wingdings"], ("Wingdings".to_string(), true, true));
        assert!(p.sizes.contains_key(&1200));
        assert!(!p.sizes.contains_key(&1050)); // szCs (complex scripts) is not listed
    }

    #[test]
    fn probe_works_for_every_format() {
        let (r, probe) = Rules::probing();
        let _ = drawingml_part(r#"<a:rPr sz="1800"><a:latin typeface="Calibri"/></a:rPr>"#, &r, false);
        let _ = excel_styles(r#"<font><sz val="11"/><name val="Cambria"/></font>"#, &r);
        let _ = odf_part(r#"<style:text-properties style:font-name="Liberation Serif" fo:font-size="12pt"/>"#, &r, CTX_TEXT);
        let _ = rtf_part(br"{\rtf1{\fonttbl{\f0 Tahoma;}}\fs20 x}", &r);
        let _ = web_text("p { font-family: Verdana, sans-serif; font-size: 16px }", &r);
        let p = probe.lock().unwrap();
        for name in ["calibri", "cambria", "liberation serif", "tahoma", "verdana"] {
            assert!(p.fonts.contains_key(name), "{}", name);
        }
        assert!(!p.fonts.contains_key("sans-serif"));
        for size in [1800, 1100, 1200, 1000] {
            assert!(p.sizes.contains_key(&size), "{}", size);
        }
    }

    fn flags(p: &Probe, hundredths: i64) -> u8 {
        *p.sizes.get(&hundredths).unwrap_or(&0)
    }

    #[test]
    fn scan_tells_where_word_sizes_sit() {
        let (r, probe) = Rules::probing();
        let doc = concat!(
            r#"<w:p><w:pPr><w:rPr><w:sz w:val="2"/></w:rPr></w:pPr>"#,
            r#"<w:r><w:rPr><w:sz w:val="24"/></w:rPr><w:t>Hello</w:t></w:r>"#,
            r#"<w:r><w:rPr><w:sz w:val="220"/></w:rPr><w:t xml:space="preserve"> </w:t></w:r>"#,
            r#"<w:r><w:rPr><w:sz w:val="28"/></w:rPr><w:tab/></w:r></w:p>"#
        );
        let _ = word_part(doc, "word/document.xml", &r);
        let styles = r#"<w:styles><w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="22"/></w:rPr></w:rPrDefault></w:docDefaults><w:style w:type="paragraph" w:styleId="X"><w:rPr><w:sz w:val="21"/><w:szCs w:val="40"/></w:rPr></w:style></w:styles>"#;
        let _ = word_part(styles, "word/styles.xml", &r);
        let p = probe.lock().unwrap();
        assert_eq!(flags(&p, 1200), CTX_TEXT); // visible text
        assert_eq!(flags(&p, 100), CTX_HIDDEN); // paragraph mark
        assert_eq!(flags(&p, 11000), CTX_HIDDEN); // run with only a space
        assert_eq!(flags(&p, 1400), CTX_HIDDEN); // run with only a tab
        assert_eq!(flags(&p, 1100), CTX_STYLE); // document default
        assert_eq!(flags(&p, 1050), CTX_STYLE); // style definition
        assert_eq!(flags(&p, 2000), 0); // szCs is ignored
    }

    #[test]
    fn scan_tells_where_pptx_sizes_sit() {
        let (r, probe) = Rules::probing();
        let slide = r#"<a:p><a:r><a:rPr sz="1800"/><a:t>Title</a:t></a:r><a:r><a:rPr sz="2400"/><a:t> </a:t></a:r><a:endParaRPr sz="1000"/></a:p>"#;
        let _ = drawingml_part(slide, &r, false);
        let master = r#"<a:lvl1pPr><a:defRPr sz="3200"/></a:lvl1pPr><a:r><a:rPr sz="4400"/><a:t>Click to edit</a:t></a:r>"#;
        let _ = drawingml_part(master, &r, true);
        let p = probe.lock().unwrap();
        assert_eq!(flags(&p, 1800), CTX_TEXT);
        assert_eq!(flags(&p, 2400), CTX_HIDDEN);
        assert_eq!(flags(&p, 1000), CTX_HIDDEN);
        assert_eq!(flags(&p, 3200), CTX_STYLE);
        assert_eq!(flags(&p, 4400), CTX_STYLE);
    }

    #[test]
    fn scan_tells_where_odf_and_rtf_sizes_sit() {
        let (r, probe) = Rules::probing();
        let _ = odf_part(r#"<style:text-properties fo:font-size="12pt" style:font-size-asian="30pt"/>"#, &r, CTX_TEXT);
        let _ = odf_part(r#"<style:text-properties fo:font-size="11pt"/>"#, &r, CTX_STYLE);
        let _ = rtf_part(br"{\rtf1{\stylesheet{\s1\fs26 Heading;}}\fs20 text}", &r);
        let p = probe.lock().unwrap();
        assert_eq!(flags(&p, 1200), CTX_TEXT);
        assert_eq!(flags(&p, 3000), 0); // asian size is ignored
        assert_eq!(flags(&p, 1100), CTX_STYLE);
        assert_eq!(flags(&p, 1300), CTX_STYLE);
        assert_eq!(flags(&p, 1000), CTX_TEXT);
    }

    #[test]
    fn any_size_rule_respects_the_minimum_but_explicit_rules_do_not() {
        let mut r = rules(&[], &[(None, 12.0)]);
        r.min_size = 6.0;
        let (out, c) = word_part(r#"<w:sz w:val="2"/><w:sz w:val="16"/>"#, "word/document.xml", &r);
        assert_eq!(out, r#"<w:sz w:val="2"/><w:sz w:val="24"/>"#);
        assert_eq!(c.sizes, 1);

        let mut r2 = rules(&[], &[(Some(1.0), 5.0)]);
        r2.min_size = 6.0;
        let (out2, _) = word_part(r#"<w:sz w:val="2"/>"#, "word/document.xml", &r2);
        assert_eq!(out2, r#"<w:sz w:val="10"/>"#);

        // sizes that cannot be expressed in points (em, %) are not "small": the any-size rule still flattens them
        let mut r3 = rules(&[], &[(None, 12.0)]);
        r3.min_size = 6.0;
        let (css, _) = web_text("h1 { font-size: 2em }", &r3);
        assert_eq!(css, "h1 { font-size: 12pt }");
    }

    #[test]
    fn nothing_to_do_changes_nothing() {
        let r = rules(&[(Some("zzz"), "Arial")], &[(Some(99.0), 10.0)]);
        let xml = r#"<w:rFonts w:ascii="Calibri"/><w:sz w:val="24"/>"#;
        let (out, c) = word_part(xml, "word/document.xml", &r);
        assert_eq!(out, xml);
        assert_eq!(c.total(), 0);
    }
}
