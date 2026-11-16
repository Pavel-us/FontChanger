//! Batch engine: walks folders, opens documents, applies the rules from `fonts.rs`, writes the files back.
//! Independent of Tauri, so it is tested on its own (`cargo test`).
//!
//! User-facing messages are returned as codes ("skip.legacy", "err.rule_empty\u{1f}3" …):
//! the part after \u{1f} is an argument. The frontend translates them via src/locales/*.json.

use crate::fonts::{self, Counts, FontRule, Probe, Rules, SizeRule};
use encoding_rs::WINDOWS_1251;
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use walkdir::WalkDir;
use zip::{write::SimpleFileOptions, CompressionMethod};

type Res<T> = Result<T, Box<dyn Error>>;

// ───────────── types exchanged with the UI ─────────────

/// One rule as typed by the user. Empty strings mean "not set": an empty source font is "any font",
/// an empty source size is "any size". Sizes are in points ("12", "10,5", "11pt").
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rule {
    pub from_font: String,
    pub to_font: String,
    pub from_size: String,
    pub to_size: String,
    /// Where the rule applies: empty means every part, otherwise one part group
    /// of a Word file ("body", "headers", "footers", "footnotes", "endnotes", "comments", "styles").
    pub scope: String,
    /// Only paragraphs of this Word style (id or built-in name, case-insensitive); empty = any.
    pub style: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Request {
    pub folder: String,
    pub mask: String,
    pub rules: Vec<Rule>,
    pub recursive: bool,
    pub backup: bool,
    pub dry: bool,
    /// "any font" rules also change fonts of Asian and complex scripts
    pub scripts: bool,
    /// "any font" rules leave Symbol, Wingdings and similar fonts alone
    pub keep_symbols: bool,
    /// 0 = no limit
    pub max_size_mb: u64,
    /// "any size" rules leave sizes below this many points alone; 0 = off
    pub min_size: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEvent {
    pub path: String,
    /// ok | skipped | error
    pub status: String,
    /// fonts + sizes
    pub count: usize,
    pub fonts: usize,
    pub sizes: usize,
    pub info: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Stats {
    pub files: usize,
    pub changed: usize,
    pub fonts: usize,
    pub sizes: usize,
    pub skipped: usize,
    pub errors: usize,
}

// ───────────── rules ─────────────

fn parse_size(s: &str) -> Result<Option<f64>, ()> {
    let t = s.trim().to_lowercase();
    let t = t.trim_end_matches("pt").trim().replace(',', ".");
    if t.is_empty() {
        return Ok(None);
    }
    let v: f64 = t.parse().map_err(|_| ())?;
    if !(1.0..=1000.0).contains(&v) {
        return Err(());
    }
    Ok(Some(v))
}

/// Characters that cannot be part of a font name and would break XML, RTF or CSS.
fn bad_font_name(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '{' | '}' | '\\' | ';' | '"' | '\'' | '<' | '>' | ',') || c.is_control())
}

pub fn compile(rules: &[Rule], o: &Request) -> Result<Rules, String> {
    if rules.is_empty() {
        return Err("err.no_rules".into());
    }
    for (i, r) in rules.iter().enumerate() {
        if !r.scope.is_empty() && !SCOPE_NAMES.contains(&r.scope.as_str()) {
            return Err(format!("err.scope\u{1f}{}", i + 1));
        }
    }
    let mut out = Rules {
        scripts: o.scripts,
        keep_symbols: o.keep_symbols,
        min_size: if o.min_size.is_finite() { o.min_size.max(0.0) } else { 0.0 },
        ..Default::default()
    };
    for (i, r) in rules.iter().enumerate() {
        let n = i + 1;
        let ff = r.from_font.trim();
        let tf = r.to_font.trim();
        let fs = parse_size(&r.from_size).map_err(|_| format!("err.size\u{1f}{n}"))?;
        let ts = parse_size(&r.to_size).map_err(|_| format!("err.size\u{1f}{n}"))?;
        if tf.is_empty() && ts.is_none() {
            return Err(format!("err.rule_empty\u{1f}{n}"));
        }
        if !ff.is_empty() && tf.is_empty() {
            return Err(format!("err.rule_font_target\u{1f}{n}"));
        }
        if fs.is_some() && ts.is_none() {
            return Err(format!("err.rule_size_target\u{1f}{n}"));
        }
        if bad_font_name(ff) || bad_font_name(tf) {
            return Err(format!("err.font_name\u{1f}{n}"));
        }
        if !tf.is_empty() {
            out.fonts.push(FontRule { from: if ff.is_empty() { None } else { Some(ff.to_lowercase()) }, to: tf.to_string() });
        }
        if let Some(t) = ts {
            out.sizes.push(SizeRule { from: fs, to: t });
        }
    }
    // specific rules first, "any" rules last (the sort is stable: the user's order is kept otherwise)
    out.fonts.sort_by_key(|r| r.from.is_none());
    out.sizes.sort_by_key(|r| r.from.is_none());
    Ok(out)
}

/// Part groups a scope may name. Excel, PowerPoint and LibreOffice files have no scoped parts:
/// a scoped rule simply applies to the whole file there.
pub const SCOPE_NAMES: [&str; 7] = ["body", "headers", "footers", "footnotes", "endnotes", "comments", "styles"];

/// Which group of parts a file entry belongs to; empty = a part no scope names.
fn part_scope(name: &str) -> &'static str {
    let Some(rest) = name.strip_prefix("word/") else {
        return "";
    };
    if rest.contains('/') || !rest.ends_with(".xml") {
        return "";
    }
    match rest {
        "document.xml" => "body",
        "footnotes.xml" => "footnotes",
        "endnotes.xml" => "endnotes",
        "comments.xml" => "comments",
        "styles.xml" => "styles",
        r if r.starts_with("header") => "headers",
        r if r.starts_with("footer") => "footers",
        _ => "",
    }
}

/// Several rule groups in priority order: what the first group decides is final, later groups only
/// handle what the earlier ones did not touch. Inside one group the usual order holds
/// (rules with a source font or size first, "any" rules last).
fn compose(groups: &[&[Rule]], o: &Request) -> Result<Rules, String> {
    let mut acc: Option<Rules> = None;
    for g in groups {
        if g.is_empty() {
            continue;
        }
        let r = compile(g, o)?;
        acc = Some(match acc.take() {
            None => r,
            Some(mut a) => {
                a.fonts.extend(r.fonts);
                a.sizes.extend(r.sizes);
                a
            }
        });
    }
    Ok(acc.unwrap_or_default())
}

/// Compiled style rules: (lower-case style key, rules for paragraphs of that style).
type Styled = Vec<(String, Rules)>;

/// Everything a run needs: the rules that apply everywhere, one set per part group that has rules of its own,
/// and one set per paragraph style that has rules of its own.
///
/// Priority, highest first:
///  1. rules limited to a paragraph style (paragraphs of that style in the body),
///  2. rules limited to an area (headers, footers, …; the body area also covers styled paragraphs),
///  3. rules that apply everywhere.
///
/// A rule with a `style` applies to paragraphs of that style only; its `scope` is not used.
fn compile_scoped(rules: &[Rule], o: &Request) -> Result<(Rules, HashMap<&'static str, Rules>, Styled), String> {
    // validate the whole list once, so that an error names the number the user sees in the list
    compile(rules, o)?;

    let plain: Vec<Rule> = rules.iter().filter(|r| r.scope.is_empty() && r.style.trim().is_empty()).cloned().collect();
    let base = compose(&[plain.as_slice()], o)?;

    let mut scoped: HashMap<&'static str, Rules> = HashMap::new();
    for g in SCOPE_NAMES {
        let own: Vec<Rule> = rules.iter().filter(|r| r.scope == g && r.style.trim().is_empty()).cloned().collect();
        if own.is_empty() {
            continue;
        }
        scoped.insert(g, compose(&[own.as_slice(), plain.as_slice()], o)?);
    }

    let body: Vec<Rule> = rules.iter().filter(|r| r.scope == "body" && r.style.trim().is_empty()).cloned().collect();
    let mut keys: Vec<String> = rules.iter().map(|r| r.style.trim().to_lowercase()).filter(|k| !k.is_empty()).collect();
    keys.sort();
    keys.dedup();
    let mut styled: Styled = Vec::new();
    for key in keys {
        let own: Vec<Rule> = rules.iter().filter(|r| r.style.trim().to_lowercase() == key).cloned().collect();
        styled.push((key, compose(&[own.as_slice(), body.as_slice(), plain.as_slice()], o)?));
    }
    Ok((base, scoped, styled))
}

// ───────────── writing files ─────────────

fn with_suffix(p: &Path, s: &str) -> PathBuf {
    let mut os = p.as_os_str().to_owned();
    os.push(s);
    PathBuf::from(os)
}

fn finish_write(path: &Path, tmp: &Path, meta: &fs::Metadata, backup: bool) -> Res<()> {
    if backup {
        fs::copy(path, with_suffix(path, ".bak"))?;
    }
    let _ = fs::set_permissions(tmp, meta.permissions());
    if let Err(e) = fs::rename(tmp, path) {
        let _ = fs::remove_file(tmp);
        return Err(e.into());
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8], meta: &fs::Metadata, backup: bool) -> Res<()> {
    let tmp = with_suffix(path, ".tmp~");
    fs::write(&tmp, bytes)?;
    finish_write(path, &tmp, meta, backup)
}

// ───────────── encodings (HTML and CSS) ─────────────

enum Enc {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Cp1251,
}

fn decode16(b: &[u8], le: bool) -> Option<String> {
    if b.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
        .collect();
    String::from_utf16(&units).ok()
}

fn decode(data: &[u8]) -> (String, Enc) {
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        if let Ok(s) = std::str::from_utf8(&data[3..]) {
            return (s.to_string(), Enc::Utf8Bom);
        }
    }
    if data.starts_with(&[0xFF, 0xFE]) {
        if let Some(s) = decode16(&data[2..], true) {
            return (s, Enc::Utf16Le);
        }
    }
    if data.starts_with(&[0xFE, 0xFF]) {
        if let Some(s) = decode16(&data[2..], false) {
            return (s, Enc::Utf16Be);
        }
    }
    if let Ok(s) = std::str::from_utf8(data) {
        return (s.to_string(), Enc::Utf8);
    }
    let (s, _) = WINDOWS_1251.decode_without_bom_handling(data);
    (s.into_owned(), Enc::Cp1251)
}

fn encode(text: &str, enc: &Enc) -> Result<Vec<u8>, String> {
    match enc {
        Enc::Utf8 => Ok(text.as_bytes().to_vec()),
        Enc::Utf8Bom => {
            let mut v = vec![0xEF, 0xBB, 0xBF];
            v.extend_from_slice(text.as_bytes());
            Ok(v)
        }
        Enc::Utf16Le => {
            let mut v = vec![0xFF, 0xFE];
            for u in text.encode_utf16() {
                v.extend_from_slice(&u.to_le_bytes());
            }
            Ok(v)
        }
        Enc::Utf16Be => {
            let mut v = vec![0xFE, 0xFF];
            for u in text.encode_utf16() {
                v.extend_from_slice(&u.to_be_bytes());
            }
            Ok(v)
        }
        Enc::Cp1251 => {
            let (b, _, bad) = WINDOWS_1251.encode(text);
            if bad {
                Err("err.cp1251".into())
            } else {
                Ok(b.into_owned())
            }
        }
    }
}

// ───────────── file kinds ─────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Word,
    Excel,
    Slides,
    Odf,
}

enum FileKind {
    Zip(Kind),
    Rtf,
    Web,
    /// .doc .xls .ppt: cannot be edited safely in place
    Legacy,
    Pdf,
    /// no fonts to change (plain text and so on): silently ignored
    Other,
}

fn file_kind(path: &Path) -> FileKind {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return FileKind::Other,
    };
    match ext.as_str() {
        "docx" | "docm" | "dotx" | "dotm" => FileKind::Zip(Kind::Word),
        "xlsx" | "xlsm" | "xltx" | "xltm" => FileKind::Zip(Kind::Excel),
        "pptx" | "pptm" | "potx" | "potm" | "ppsx" | "ppsm" => FileKind::Zip(Kind::Slides),
        "odt" | "ott" | "ods" | "ots" | "odp" | "otp" | "odg" | "otg" => FileKind::Zip(Kind::Odf),
        "rtf" => FileKind::Rtf,
        "html" | "htm" | "xhtml" | "css" => FileKind::Web,
        "doc" | "dot" | "xls" | "xlt" | "ppt" | "pps" => FileKind::Legacy,
        "pdf" => FileKind::Pdf,
        _ => FileKind::Other,
    }
}

// ───────────── zipped documents ─────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Route {
    Word,
    Theme,
    ExcelStyles,
    ExcelRich,
    ExcelSheet,
    /// text of slides, notes, SmartArt, Excel text boxes
    DrawingMl,
    /// masters, layouts, presentation.xml: styles only
    DrawingStyles,
    Odf,
}

fn is_word_part(name: &str) -> bool {
    let rest = match name.strip_prefix("word/") {
        Some(r) => r,
        None => return false,
    };
    if rest.contains('/') || !rest.ends_with(".xml") {
        return false;
    }
    matches!(rest, "document.xml" | "styles.xml" | "numbering.xml" | "footnotes.xml" | "endnotes.xml" | "comments.xml")
        || rest.starts_with("header")
        || rest.starts_with("footer")
}

fn slide_route(name: &str) -> Option<Route> {
    if !name.ends_with(".xml") {
        return None;
    }
    if name == "ppt/presentation.xml" {
        return Some(Route::DrawingStyles);
    }
    let dirs = [
        ("ppt/slides/", Route::DrawingMl),
        ("ppt/notesSlides/", Route::DrawingMl),
        ("ppt/slideLayouts/", Route::DrawingStyles),
        ("ppt/slideMasters/", Route::DrawingStyles),
        ("ppt/notesMasters/", Route::DrawingStyles),
        ("ppt/handoutMasters/", Route::DrawingStyles),
    ];
    for (dir, rt) in dirs {
        if let Some(rest) = name.strip_prefix(dir) {
            return if rest.contains('/') { None } else { Some(rt) };
        }
    }
    // SmartArt text lives in data*.xml and drawing*.xml
    if let Some(rest) = name.strip_prefix("ppt/diagrams/") {
        if !rest.contains('/') && (rest.starts_with("data") || rest.starts_with("drawing")) {
            return Some(Route::DrawingMl);
        }
    }
    None
}

/// Chart and SmartArt parts. Their text properties are DrawingML (`a:rPr`, `a:defRPr`, `a:latin typeface`),
/// so slides' code handles them too. `colors*.xml` and `style*.xml` hold no fonts and are left out.
fn is_chart_or_diagram(name: &str) -> bool {
    let dirs = ["word/charts/", "word/diagrams/", "xl/charts/", "ppt/charts/", "ppt/diagrams/"];
    let Some(rest) = dirs.iter().find_map(|d| name.strip_prefix(d)) else {
        return false;
    };
    !rest.contains('/')
        && rest.ends_with(".xml")
        && (rest.starts_with("chart") || rest.starts_with("data") || rest.starts_with("drawing"))
}

fn route(kind: Kind, name: &str) -> Option<Route> {
    match kind {
        Kind::Word => {
            if name.starts_with("word/theme/") && name.ends_with(".xml") {
                Some(Route::Theme)
            } else if is_word_part(name) {
                Some(Route::Word)
            } else if is_chart_or_diagram(name) {
                Some(Route::DrawingMl)
            } else {
                None
            }
        }
        Kind::Excel => {
            if name == "xl/styles.xml" {
                Some(Route::ExcelStyles)
            } else if name == "xl/sharedStrings.xml" || (name.starts_with("xl/comments") && name.ends_with(".xml")) {
                Some(Route::ExcelRich)
            } else if let Some(rest) = name.strip_prefix("xl/worksheets/") {
                if rest.ends_with(".xml") && !rest.contains('/') {
                    Some(Route::ExcelSheet)
                } else {
                    None
                }
            } else if name.starts_with("xl/theme/") && name.ends_with(".xml") {
                Some(Route::Theme)
            } else if let Some(rest) = name.strip_prefix("xl/drawings/") {
                if rest.starts_with("drawing") && rest.ends_with(".xml") && !rest.contains('/') {
                    Some(Route::DrawingMl)
                } else {
                    None
                }
            } else if is_chart_or_diagram(name) {
                Some(Route::DrawingMl)
            } else {
                None
            }
        }
        Kind::Slides => {
            if name.starts_with("ppt/theme/") && name.ends_with(".xml") {
                Some(Route::Theme)
            } else if is_chart_or_diagram(name) {
                Some(Route::DrawingMl)
            } else {
                slide_route(name)
            }
        }
        Kind::Odf => {
            if name == "content.xml" || name == "styles.xml" {
                Some(Route::Odf)
            } else {
                None
            }
        }
    }
}

fn process_part(route: Route, name: &str, xml: &str, r: &Rules) -> (String, Counts) {
    match route {
        Route::Word => fonts::word_part(xml, name, r),
        Route::Theme => fonts::theme_part(xml, r),
        Route::ExcelStyles => fonts::excel_styles(xml, r),
        Route::ExcelRich => fonts::excel_rich(xml, r),
        // worksheets can be huge and rarely contain rich text
        Route::ExcelSheet => {
            if xml.contains("<rPr>") {
                fonts::excel_rich(xml, r)
            } else {
                (String::new(), Counts::default())
            }
        }
        Route::DrawingMl => fonts::drawingml_part(xml, r, false),
        Route::DrawingStyles => fonts::drawingml_part(xml, r, true),
        Route::Odf => fonts::odf_part(xml, r, if name == "content.xml" { fonts::CTX_TEXT } else { fonts::CTX_STYLE }),
    }
}

enum Outcome {
    Unchanged,
    Changed(Counts),
    Skipped(String),
}

/// What `word/styles.xml` says about paragraph styles.
#[derive(Default)]
struct StyleInfo {
    /// styleId (lower-case) -> style name (lower-case)
    names: HashMap<String, String>,
    /// id (lower-case) of the style that paragraphs without `w:pStyle` have ("Normal" in nearly every file)
    default_id: String,
}

fn read_style_info(ar: &mut zip::ZipArchive<fs::File>) -> StyleInfo {
    let mut info = StyleInfo { default_id: "normal".to_string(), ..Default::default() };
    let Ok(mut f) = ar.by_name("word/styles.xml") else {
        return info;
    };
    let mut xml = String::new();
    if f.read_to_string(&mut xml).is_err() {
        return info;
    }
    let (Ok(style), Ok(id), Ok(name)) = (
        Regex::new(r"(?s)<w:style\b([^>]*)>(.*?)</w:style>"),
        Regex::new(r#"w:styleId="([^"]+)""#),
        Regex::new(r#"<w:name\s+w:val="([^"]+)""#),
    ) else {
        return info;
    };
    for caps in style.captures_iter(&xml) {
        let attrs = &caps[1];
        let Some(idc) = id.captures(attrs) else {
            continue;
        };
        let key = idc[1].to_lowercase();
        if attrs.contains(r#"w:type="paragraph""#) && attrs.contains(r#"w:default="1""#) {
            info.default_id = key.clone();
        }
        let value = name.captures(&caps[2]).map(|c| c[1].to_lowercase()).unwrap_or_default();
        info.names.insert(key, value);
    }
    info
}

fn push_processed(out: &mut String, total: &mut Counts, chunk: &str, rules: &Rules) {
    let (changed, c) = fonts::word_part(chunk, "word/document.xml", rules);
    out.push_str(&changed);
    total.add(c);
}

/// The body of a Word file when some rules are limited to a paragraph style.
/// A paragraph of such a style is handled by that style's rules (which come first) and nothing else touches it;
/// every other paragraph, and everything between paragraphs, is handled by `base`.
/// A style is matched by its id (`Heading1`) or by its name (`heading 1`), case-insensitively;
/// paragraphs without `w:pStyle` have the default style (usually `Normal`).
fn process_document_styled(xml: &str, base: &Rules, styled: &[(String, Rules)], info: &StyleInfo) -> (String, Counts) {
    let (Ok(para), Ok(ps)) = (
        Regex::new(r"(?s)<w:p(?:\s(?:[^>/]|/[^>])*)?>.*?</w:p>"),
        Regex::new(r#"<w:pStyle\s+w:val="([^"]+)""#),
    ) else {
        return fonts::word_part(xml, "word/document.xml", base);
    };
    let mut total = Counts::default();
    let mut out = String::with_capacity(xml.len() + 64);
    let mut last = 0;
    for m in para.find_iter(xml) {
        let text = m.as_str();
        let id = match ps.captures(text) {
            Some(c) => c[1].to_lowercase(),
            None => info.default_id.clone(),
        };
        let name = info.names.get(&id).cloned().unwrap_or_default();
        let rules = styled
            .iter()
            .find(|(key, _)| *key == id || (!name.is_empty() && *key == name))
            .map(|(_, r)| r);
        let Some(rules) = rules else {
            continue; // stays in the stretch that `base` handles
        };
        push_processed(&mut out, &mut total, &xml[last..m.start()], base);
        push_processed(&mut out, &mut total, text, rules);
        last = m.end();
    }
    push_processed(&mut out, &mut total, &xml[last..], base);
    (out, total)
}

fn process_zip_doc(
    kind: Kind,
    path: &Path,
    meta: &fs::Metadata,
    r: &Rules,
    scoped: &HashMap<&'static str, Rules>,
    styled: &[(String, Rules)],
    o: &Request,
) -> Res<Outcome> {
    let mut ar = zip::ZipArchive::new(fs::File::open(path)?)?;
    // style names are only read when a rule is limited to a paragraph style
    let info = if kind == Kind::Word && !styled.is_empty() { read_style_info(&mut ar) } else { StyleInfo::default() };
    // (name, data, is_dir, compression) — order and compression are preserved (matters for ODF: mimetype)
    let mut items: Vec<(String, Vec<u8>, bool, CompressionMethod)> = Vec::new();
    let mut total = Counts::default();
    for i in 0..ar.len() {
        let mut f = ar.by_index(i)?;
        let name = f.name().to_string();
        let is_dir = f.is_dir();
        let method = f.compression();
        let mut buf = Vec::new();
        if !is_dir {
            f.read_to_end(&mut buf)?;
            if let Some(rt) = route(kind, &name) {
                if let Ok(xml) = std::str::from_utf8(&buf) {
                    // a scoped rule only applies inside its own parts; everything else applies everywhere
                    let rule_set = scoped.get(part_scope(&name)).unwrap_or(r);
                    let (new, c) = if kind == Kind::Word && name == "word/document.xml" && !styled.is_empty() {
                        process_document_styled(xml, rule_set, styled, &info)
                    } else {
                        process_part(rt, &name, xml, rule_set)
                    };
                    if c.total() > 0 {
                        total.add(c);
                        buf = new.into_bytes();
                    }
                }
            }
        }
        items.push((name, buf, is_dir, method));
    }
    drop(ar);
    if total.total() == 0 {
        return Ok(Outcome::Unchanged);
    }
    if !o.dry {
        let tmp = with_suffix(path, ".tmp~");
        {
            let mut zw = zip::ZipWriter::new(fs::File::create(&tmp)?);
            for (name, data, is_dir, method) in &items {
                let opts = SimpleFileOptions::default().compression_method(*method);
                if *is_dir {
                    zw.add_directory(name.as_str(), opts)?;
                } else {
                    zw.start_file(name.as_str(), opts)?;
                    zw.write_all(data)?;
                }
            }
            zw.finish()?;
        }
        finish_write(path, &tmp, meta, o.backup)?;
    }
    Ok(Outcome::Changed(total))
}

// ───────────── RTF, HTML, CSS ─────────────

fn process_rtf(path: &Path, meta: &fs::Metadata, r: &Rules, o: &Request) -> Res<Outcome> {
    let data = fs::read(path)?;
    let start = if data.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    if !data[start..].starts_with(b"{\\rtf") {
        return Ok(Outcome::Skipped("skip.not_rtf".into()));
    }
    let (new, c) = fonts::rtf_part(&data, r);
    if c.total() == 0 {
        return Ok(Outcome::Unchanged);
    }
    if new != data && !o.dry {
        write_atomic(path, &new, meta, o.backup)?;
    }
    Ok(Outcome::Changed(c))
}

fn process_web(path: &Path, meta: &fs::Metadata, r: &Rules, o: &Request) -> Res<Outcome> {
    let data = fs::read(path)?;
    if data.is_empty() {
        return Ok(Outcome::Unchanged);
    }
    let utf16_bom = data.starts_with(&[0xFF, 0xFE]) || data.starts_with(&[0xFE, 0xFF]);
    if !utf16_bom && data[..data.len().min(8192)].contains(&0) {
        return Ok(Outcome::Skipped("skip.binary".into()));
    }
    let (text, enc) = decode(&data);
    let (new, c) = fonts::web_text(&text, r);
    if c.total() == 0 {
        return Ok(Outcome::Unchanged);
    }
    if new != text && !o.dry {
        let bytes = encode(&new, &enc)?;
        write_atomic(path, &bytes, meta, o.backup)?;
    }
    Ok(Outcome::Changed(c))
}

// ───────────── folder walk ─────────────

fn glob(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn parse_masks(mask: &str) -> Vec<Vec<char>> {
    let masks: Vec<Vec<char>> = mask
        .split(|c: char| c == ';' || c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase().chars().collect())
        .collect();
    if masks.is_empty() {
        vec![vec!['*']]
    } else {
        masks
    }
}

fn process_file(
    path: &Path,
    r: &Rules,
    scoped: &HashMap<&'static str, Rules>,
    styled: &[(String, Rules)],
    o: &Request,
) -> Result<Outcome, String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if o.max_size_mb > 0 && meta.len() > o.max_size_mb.saturating_mul(1024 * 1024) {
        return Ok(Outcome::Skipped(format!("skip.too_big\u{1f}{}", o.max_size_mb)));
    }
    let res = match file_kind(path) {
        FileKind::Legacy => return Ok(Outcome::Skipped("skip.legacy".into())),
        FileKind::Pdf => return Ok(Outcome::Skipped("skip.pdf".into())),
        FileKind::Other => return Ok(Outcome::Unchanged),
        FileKind::Zip(kind) => process_zip_doc(kind, path, &meta, r, scoped, styled, o),
        FileKind::Rtf => process_rtf(path, &meta, r, o),
        FileKind::Web => process_web(path, &meta, r, o),
    };
    res.map_err(|e| e.to_string())
}

/// Files that a run or a scan would look at: supported formats whose name matches the mask.
fn list_files(o: &Request) -> Result<Vec<PathBuf>, String> {
    let root = Path::new(&o.folder);
    if !root.is_dir() {
        return Err("err.folder_not_found".into());
    }
    let masks = parse_masks(&o.mask);
    let depth = if o.recursive { usize::MAX } else { 1 };

    Ok(WalkDir::new(root)
        .max_depth(depth)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && e.file_name() == ".git"))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            // skip Office/LibreOffice lock files and our own temporary files
            let n = e.file_name().to_string_lossy();
            !(n.starts_with("~$") || n.starts_with(".~lock.") || n.ends_with(".tmp~"))
        })
        .filter(|e| !matches!(file_kind(e.path()), FileKind::Other))
        .filter(|e| {
            let name: Vec<char> = e.file_name().to_string_lossy().to_lowercase().chars().collect();
            masks.iter().any(|m| glob(m, &name))
        })
        .map(|e| e.into_path())
        .collect())
}

pub fn run<F>(o: &Request, emit: F) -> Result<Stats, String>
where
    F: Fn(FileEvent) + Sync,
{
    let (rules, scoped, styled) = compile_scoped(&o.rules, o)?;
    let files = list_files(o)?;

    let changed = AtomicUsize::new(0);
    let font_total = AtomicUsize::new(0);
    let size_total = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);

    let pool = rayon::ThreadPoolBuilder::new()
        .stack_size(16 * 1024 * 1024)
        .build()
        .map_err(|e| e.to_string())?;

    pool.install(|| {
        files.par_iter().for_each(|p| {
            let path = p.to_string_lossy().to_string();
            // a bug in one file must not take the whole application down
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process_file(p, &rules, &scoped, &styled, o)))
                .unwrap_or_else(|_| Err("err.internal".to_string()));
            match res {
                Ok(Outcome::Unchanged) => {}
                Ok(Outcome::Changed(c)) => {
                    changed.fetch_add(1, Ordering::Relaxed);
                    font_total.fetch_add(c.fonts, Ordering::Relaxed);
                    size_total.fetch_add(c.sizes, Ordering::Relaxed);
                    emit(FileEvent {
                        path,
                        status: "ok".into(),
                        count: c.total(),
                        fonts: c.fonts,
                        sizes: c.sizes,
                        info: String::new(),
                    });
                }
                Ok(Outcome::Skipped(why)) => {
                    skipped.fetch_add(1, Ordering::Relaxed);
                    emit(FileEvent { path, status: "skipped".into(), count: 0, fonts: 0, sizes: 0, info: why });
                }
                Err(e) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    emit(FileEvent { path, status: "error".into(), count: 0, fonts: 0, sizes: 0, info: e });
                }
            }
        });
    });

    Ok(Stats {
        files: files.len(),
        changed: changed.load(Ordering::Relaxed),
        fonts: font_total.load(Ordering::Relaxed),
        sizes: size_total.load(Ordering::Relaxed),
        skipped: skipped.load(Ordering::Relaxed),
        errors: errors.load(Ordering::Relaxed),
    })
}

// ───────────── verify: what survived a run? ─────────────

/// Something a run could not change: the same rules would change it again.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Residual {
    pub path: String,
    /// fonts that still match a rule, as they are written in the file
    pub fonts: Vec<String>,
    /// sizes in points that still match a rule
    pub sizes: Vec<f64>,
    /// where the text probably hides: "embeddings", "media" or empty
    pub hint: String,
}

/// Lists what the rules would still change in the files they select. Nothing is written.
/// Run it after a real run: whatever is left is text the engine cannot reach
/// (an embedded object, a picture with text, a chart in an old format).
pub fn verify(o: &Request) -> Result<Vec<Residual>, String> {
    // The check works per file, not per part, so scoped rules are left out:
    // reporting them here would claim leftovers that a scoped run leaves on purpose.
    let plain: Vec<Rule> = o
        .rules
        .iter()
        .filter(|r| r.scope.is_empty() && r.style.trim().is_empty())
        .cloned()
        .collect();
    if plain.is_empty() {
        return Ok(Vec::new());
    }
    let rules = compile(&plain, o)?;
    let files = list_files(o)?;
    let found = Mutex::new(Vec::new());

    let pool = rayon::ThreadPoolBuilder::new()
        .stack_size(16 * 1024 * 1024)
        .build()
        .map_err(|e| e.to_string())?;

    pool.install(|| {
        files.par_iter().for_each(|p| {
            let Ok(Some(probe)) = scan_file(p, o) else {
                return;
            };
            let mut fonts: Vec<String> = probe
                .fonts
                .values()
                .filter(|(name, latin, symbol)| rules.font_for_ext(name, *latin, *symbol).is_some())
                .map(|(name, _, _)| name.clone())
                .collect();
            let mut sizes: Vec<f64> = probe
                .sizes
                .iter()
                .filter(|(&h, &ctx)| rules.size_for_ctx(Some(h as f64 / 100.0), ctx).is_some())
                .map(|(&h, _)| h as f64 / 100.0)
                .collect();
            if fonts.is_empty() && sizes.is_empty() {
                return;
            }
            fonts.sort();
            fonts.dedup();
            sizes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            sizes.dedup();
            if let Ok(mut g) = found.lock() {
                g.push(Residual { path: p.to_string_lossy().to_string(), fonts, sizes, hint: leftover_hint(p) });
            }
        });
    });

    let mut v = found.into_inner().unwrap_or_default();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

/// Where text can hide from the engine: embedded objects and pictures.
fn leftover_hint(path: &Path) -> String {
    if !matches!(file_kind(path), FileKind::Zip(_)) {
        return String::new();
    }
    let Ok(mut z) = zip::ZipArchive::new(match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    }) else {
        return String::new();
    };
    let (mut emb, mut media) = (false, false);
    for i in 0..z.len() {
        let Ok(f) = z.by_index(i) else { continue };
        let n = f.name();
        if n.contains("/embeddings/") {
            emb = true;
        } else if n.contains("/media/") {
            media = true;
        }
    }
    if emb {
        "embeddings".into()
    } else if media {
        "media".into()
    } else {
        String::new()
    }
}

// ───────────── missing system fonts ─────────────

/// A font the selected files use that this machine does not have.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MissingFont {
    pub name: String,
    /// how many files use it
    pub files: usize,
}

/// Family names installed here, lower-cased.
pub fn installed_families() -> HashSet<String> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    db.faces()
        .flat_map(|f| f.families.iter().map(|(name, _)| name.to_lowercase()))
        .collect()
}

/// Pure part of the check, so it can be tested without a system font list.
fn missing(found: &[FontInfo], installed: &HashSet<String>) -> Vec<MissingFont> {
    let mut out: Vec<MissingFont> = found
        .iter()
        .filter(|f| !f.symbol && !f.invalid)
        .filter(|f| !installed.contains(&f.name.to_lowercase()))
        .map(|f| MissingFont { name: f.name.clone(), files: f.files })
        .collect();
    out.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Scans the folder and lists the fonts that are not installed on this machine.
pub fn missing_fonts<C>(o: &Request, cancelled: C) -> Result<Vec<MissingFont>, String>
where
    C: Fn() -> bool + Sync,
{
    let result = scan(o, cancelled)?;
    Ok(missing(&result.fonts, &installed_families()))
}

// ───────────── sample: copy font and size from a reference document ─────────────

#[derive(Debug, Clone, Serialize, Default)]
pub struct Sample {
    /// most used first: font name as it is written in the file, and how many slots use it
    pub fonts: Vec<(String, usize)>,
    /// most used first: size in points, and how many slots use it
    pub sizes: Vec<(f64, usize)>,
}

/// Counts how many font and size slots one file has for the given rules.
fn count_in_file(path: &Path, r: &Rules) -> Result<Counts, String> {
    match file_kind(path) {
        FileKind::Zip(kind) => count_zip_doc(kind, path, r).map_err(|e| e.to_string()),
        FileKind::Rtf => {
            let data = fs::read(path).map_err(|e| e.to_string())?;
            Ok(fonts::rtf_part(&data, r).1)
        }
        FileKind::Web => {
            let data = fs::read(path).map_err(|e| e.to_string())?;
            let (text, _) = decode(&data);
            Ok(fonts::web_text(&text, r).1)
        }
        _ => Ok(Counts::default()),
    }
}

/// Same walk as `scan_zip_doc`, but keeps the counts: one rule, one file.
fn count_zip_doc(kind: Kind, path: &Path, r: &Rules) -> Res<Counts> {
    let mut total = Counts::default();
    let mut ar = zip::ZipArchive::new(fs::File::open(path)?)?;
    for i in 0..ar.len() {
        let mut f = ar.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().to_string();
        if let Some(rt) = route(kind, &name) {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            if let Ok(xml) = std::str::from_utf8(&buf) {
                let (_, c) = process_part(rt, &name, xml, r);
                total.fonts += c.fonts;
                total.sizes += c.sizes;
            }
        }
    }
    Ok(total)
}

/// Reads one document and returns the fonts and sizes it uses, most used first.
/// The UI takes the first entry of each list as "the look of this document".
pub fn sample(path: &str) -> Result<Sample, String> {
    let p = Path::new(path);
    let probe = scan_file(p, &Request { max_size_mb: 0, ..Default::default() })?
        .ok_or_else(|| "err.sample_empty".to_string())?;

    let mut fonts = Vec::new();
    for (low, (name, latin, symbol)) in &probe.fonts {
        // symbol fonts and fonts of Asian scripts are not what a template is about
        if *symbol || !*latin {
            continue;
        }
        let r = Rules {
            fonts: vec![FontRule { from: Some(low.clone()), to: "sample".into() }],
            ..Default::default()
        };
        let c = count_in_file(p, &r)?;
        if c.fonts > 0 {
            fonts.push((name.clone(), c.fonts));
        }
    }
    fonts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut sizes = Vec::new();
    for (&h, &flags) in &probe.sizes {
        if flags & fonts::CTX_HIDDEN != 0 && flags & (fonts::CTX_TEXT | fonts::CTX_STYLE) == 0 {
            continue; // paragraph marks and other invisible sizes are not "the look of the document"
        }
        let pt = h as f64 / 100.0;
        let r = Rules {
            sizes: vec![SizeRule { from: Some(pt), to: pt + 1.0 }],
            ..Default::default()
        };
        let c = count_in_file(p, &r)?;
        if c.sizes > 0 {
            sizes.push((pt, c.sizes));
        }
    }
    sizes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)));

    Ok(Sample { fonts, sizes })
}

// ───────────── report: the results table as a file ─────────────

/// One row of the results table, as the UI sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportRow {
    pub path: String,
    pub count: String,
    pub status: String,
}

/// Writes the results as CSV. UTF-8 with a BOM, so Excel shows Cyrillic as Cyrillic.
pub fn write_report(path: &str, rows: &[ReportRow]) -> Result<String, String> {
    fn esc(s: &str) -> String {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
    let mut out = String::from("\u{feff}file,count,status\r\n");
    for r in rows {
        out.push_str(&format!("{},{},{}\r\n", esc(&r.path), esc(&r.count), esc(&r.status)));
    }
    fs::write(path, out).map_err(|e| e.to_string())?;
    Ok(path.to_string())
}

// ───────────── scan: which fonts and sizes are there? ─────────────

#[derive(Debug, Clone, Serialize, Default)]
pub struct FontInfo {
    pub name: String,
    /// number of files that contain it
    pub files: usize,
    /// Symbol, Wingdings…: "any font" rules never touch it
    pub symbol: bool,
    /// only used for Asian / complex scripts
    pub other_script: bool,
    /// not a real font name ("inherit" left over from a copy from a web page)
    pub invalid: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SizeInfo {
    /// points
    pub size: f64,
    /// files that contain this size anywhere
    pub files: usize,
    /// files where it is the size of visible text (or where the format does not say)
    pub text: usize,
    /// files where it is defined in a style, a default or a list
    pub style: usize,
    /// files where it sits on something invisible (paragraph mark, empty run)
    pub hidden: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ScanResult {
    /// documents that were read
    pub files: usize,
    pub fonts: Vec<FontInfo>,
    pub sizes: Vec<SizeInfo>,
}

struct FontAcc {
    name: String,
    files: usize,
    latin: bool,
    symbol: bool,
}

#[derive(Default)]
struct SizeAcc {
    files: usize,
    text: usize,
    style: usize,
    hidden: usize,
}

#[derive(Default)]
struct Acc {
    files: usize,
    fonts: HashMap<String, FontAcc>,
    sizes: HashMap<i64, SizeAcc>,
}

/// Names that Word keeps from a copy out of a web page but that are not fonts.
fn is_invalid_font_name(low: &str) -> bool {
    matches!(low, "inherit" | "initial" | "unset" | "revert" | "default")
}

impl Acc {
    fn add(&mut self, p: Probe) {
        self.files += 1;
        for (key, (name, latin, symbol)) in p.fonts {
            let e = self.fonts.entry(key).or_insert(FontAcc { name, files: 0, latin: false, symbol: false });
            e.files += 1;
            e.latin = e.latin || latin;
            e.symbol = e.symbol || symbol;
        }
        for (size, flags) in p.sizes {
            let e = self.sizes.entry(size).or_default();
            e.files += 1;
            if flags & (fonts::CTX_TEXT | fonts::CTX_UNKNOWN) != 0 {
                e.text += 1;
            }
            if flags & fonts::CTX_STYLE != 0 {
                e.style += 1;
            }
            if flags & fonts::CTX_HIDDEN != 0 {
                e.hidden += 1;
            }
        }
    }

    fn finish(self) -> ScanResult {
        let mut fonts: Vec<FontInfo> = self
            .fonts
            .into_values()
            .map(|f| {
                let invalid = is_invalid_font_name(&f.name.to_lowercase());
                FontInfo { name: f.name, files: f.files, symbol: f.symbol, other_script: !f.latin, invalid }
            })
            .collect();
        // the most widespread first
        fonts.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        let mut sizes: Vec<SizeInfo> =
            self.sizes
                .into_iter()
                .map(|(h, a)| SizeInfo { size: h as f64 / 100.0, files: a.files, text: a.text, style: a.style, hidden: a.hidden })
                .collect();
        sizes.sort_by(|a, b| a.size.partial_cmp(&b.size).unwrap_or(std::cmp::Ordering::Equal));
        ScanResult { files: self.files, fonts, sizes }
    }
}

fn scan_zip_doc(kind: Kind, path: &Path, r: &Rules) -> Res<()> {
    let mut ar = zip::ZipArchive::new(fs::File::open(path)?)?;
    for i in 0..ar.len() {
        let mut f = ar.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().to_string();
        if let Some(rt) = route(kind, &name) {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            if let Ok(xml) = std::str::from_utf8(&buf) {
                let _ = process_part(rt, &name, xml, r);
            }
        }
    }
    Ok(())
}

/// Reads one file with the probing rules; `None` = the file was skipped.
fn scan_file(path: &Path, o: &Request) -> Result<Option<Probe>, String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if o.max_size_mb > 0 && meta.len() > o.max_size_mb.saturating_mul(1024 * 1024) {
        return Ok(None);
    }
    let (rules, probe) = Rules::probing();
    match file_kind(path) {
        FileKind::Zip(kind) => scan_zip_doc(kind, path, &rules).map_err(|e| e.to_string())?,
        FileKind::Rtf => {
            let data = fs::read(path).map_err(|e| e.to_string())?;
            let start = if data.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
            if !data[start..].starts_with(b"{\\rtf") {
                return Ok(None);
            }
            let _ = fonts::rtf_part(&data, &rules);
        }
        FileKind::Web => {
            let data = fs::read(path).map_err(|e| e.to_string())?;
            let utf16_bom = data.starts_with(&[0xFF, 0xFE]) || data.starts_with(&[0xFE, 0xFF]);
            if data.is_empty() || (!utf16_bom && data[..data.len().min(8192)].contains(&0)) {
                return Ok(None);
            }
            let (text, _) = decode(&data);
            let _ = fonts::web_text(&text, &rules);
        }
        _ => return Ok(None),
    }
    let found = probe.lock().map_err(|_| "err.internal".to_string())?;
    Ok(Some(Probe { fonts: found.fonts.clone(), sizes: found.sizes.clone() }))
}

/// Lists the fonts and sizes used in the files that `o.folder` / `o.mask` select. Nothing is written.
/// `cancelled` is polled between files; a cancelled scan returns `Err("cancelled")`.
pub fn scan<C>(o: &Request, cancelled: C) -> Result<ScanResult, String>
where
    C: Fn() -> bool + Sync,
{
    let files = list_files(o)?;
    let acc = Mutex::new(Acc::default());
    let pool = rayon::ThreadPoolBuilder::new()
        .stack_size(16 * 1024 * 1024)
        .build()
        .map_err(|e| e.to_string())?;
    pool.install(|| {
        files.par_iter().for_each(|p| {
            if cancelled() {
                return;
            }
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_file(p, o))).unwrap_or(Ok(None));
            if let Ok(Some(found)) = res {
                if let Ok(mut a) = acc.lock() {
                    a.add(found);
                }
            }
        });
    });
    if cancelled() {
        return Err("cancelled".into());
    }
    let acc = acc.into_inner().map_err(|_| "err.internal".to_string())?;
    Ok(acc.finish())
}

// ───────────── rule templates (.fctpl) ─────────────

pub const TEMPLATE_FORMAT: &str = "fctpl";
pub const TEMPLATE_VERSION: u32 = 2;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TemplateOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_symbols: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scripts: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_size: Option<f64>,
}

/// A `.fctpl` file is JSON: `{ "format": "fctpl", "version": 2, "name": …, "mask": …, "options": {…}, "rules": […] }`.
/// Unknown fields are ignored, so newer files with extra data still load.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Template {
    pub format: String,
    pub version: u32,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<String>,
    pub options: TemplateOptions,
    pub rules: Vec<Rule>,
}

/// Writes a template; returns the path actually used (".fctpl" is added when the name has no extension).
pub fn save_template(path: &str, mut tpl: Template) -> Result<String, String> {
    // do not save rules that could not be run
    compile(&tpl.rules, &Request::default())?;
    let mut p = PathBuf::from(path);
    if p.extension().is_none() {
        p.set_extension(TEMPLATE_FORMAT);
    }
    tpl.format = TEMPLATE_FORMAT.to_string();
    tpl.version = TEMPLATE_VERSION;
    if tpl.name.trim().is_empty() {
        tpl.name = p.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    }
    let json = serde_json::to_string_pretty(&tpl).map_err(|e| e.to_string())?;
    fs::write(&p, json + "\n").map_err(|e| e.to_string())?;
    Ok(p.to_string_lossy().to_string())
}

/// Reads a `.fctpl` template. A plain tab-separated rules file (one rule per line:
/// `from font<TAB>to font<TAB>from size<TAB>to size`, `#` = comment) is accepted as well.
pub fn load_template(path: &str) -> Result<Template, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&data);
    let text = text.trim_start_matches('\u{feff}');

    if text.trim_start().starts_with('{') {
        let tpl: Template = serde_json::from_str(text).map_err(|e| format!("err.template_parse\u{1f}{e}"))?;
        if tpl.format != TEMPLATE_FORMAT {
            return Err("err.template_format".into());
        }
        if tpl.version > TEMPLATE_VERSION {
            return Err(format!("err.template_version\u{1f}{}", tpl.version));
        }
        if tpl.rules.is_empty() {
            return Err("err.no_rules_in_file".into());
        }
        return Ok(tpl);
    }

    let mut rules = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let col = |i: usize| cols.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
        rules.push(Rule { from_font: col(0), to_font: col(1), from_size: col(2), to_size: col(3), ..Default::default() });
    }
    if rules.is_empty() {
        return Err("err.no_rules_in_file".into());
    }
    Ok(Template { format: TEMPLATE_FORMAT.into(), version: TEMPLATE_VERSION, rules, ..Default::default() })
}

// ───────────── command line ─────────────

/// A headless run for scripts and schedulers:
/// `font-changer --folder DIR --preset rules.fctpl [--report out.csv] [--mask M] [--dry] [--no-backup] [--flat]`
#[derive(Debug, Default)]
pub struct Cli {
    pub request: Request,
    pub report: Option<String>,
}

/// Only these arguments start the headless mode. Anything else (an argument added by the system or by a launcher)
/// must not stop the window from opening.
pub fn wants_cli(args: &[String]) -> bool {
    args.iter().any(|a| matches!(a.as_str(), "--folder" | "--preset" | "--help" | "-h"))
}

pub fn cli_usage() -> &'static str {
    "Font Changer without a window:\n\
     \x20 font-changer --folder DIR --preset FILE.fctpl [--report OUT.csv] [--mask MASK] [--dry] [--no-backup] [--flat]\n\
     \n\
     \x20 --folder DIR       folder to process (required)\n\
     \x20 --preset FILE      .fctpl with the rules (required); the mask and the options come from it\n\
     \x20 --report FILE      write the results as CSV\n\
     \x20 --mask MASK        use another mask, e.g. \"*.docx\"\n\
     \x20 --dry              preview, nothing is written\n\
     \x20 --no-backup        do not create .bak copies\n\
     \x20 --flat             do not look into subfolders\n\
     \n\
     Exit code: 0 = done, 1 = some files failed, 2 = wrong arguments or rules."
}

/// Readable text for a message code ("err.cli_usage\u{1f}--x"); unknown codes are returned as they are.
pub fn cli_message(code: &str) -> String {
    let (key, arg) = match code.split_once('\u{1f}') {
        Some((k, a)) => (k, a),
        None => (code, ""),
    };
    match key {
        "err.cli_usage" => format!("Missing or unknown argument: {arg}\n\n{}", cli_usage()),
        "err.cli_value" => format!("{arg} needs a value\n\n{}", cli_usage()),
        "err.folder_not_found" => "The folder was not found".to_string(),
        "err.no_rules" | "err.no_rules_in_file" => "The preset contains no rules".to_string(),
        "err.template_format" => "The preset is not a Font Changer template (.fctpl)".to_string(),
        "err.template_version" => format!("The preset was made by a newer version of Font Changer (format {arg})"),
        "err.template_parse" => format!("The preset file is damaged: {arg}"),
        "err.rule_empty" => format!("Rule {arg}: nothing to change, set a new font or a new size"),
        "err.rule_font_target" => format!("Rule {arg}: a source font is set but the new font is empty"),
        "err.rule_size_target" => format!("Rule {arg}: a source size is set but the new size is empty"),
        "err.size" => format!("Rule {arg}: the size must be a number from 1 to 1000"),
        "err.font_name" => format!("Rule {arg}: the font name contains characters that are not allowed"),
        "err.scope" => format!("Rule {arg}: unknown area"),
        _ => code.replace('\u{1f}', ": "),
    }
}

fn arg_value(args: &[String], i: &mut usize, key: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i).cloned().ok_or_else(|| format!("err.cli_value\u{1f}{key}"))
}

/// Parses the command line. `None` = no arguments at all, so the window should open.
pub fn parse_cli(args: &[String]) -> Result<Option<Cli>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut cli = Cli {
        request: Request { backup: true, recursive: true, ..Default::default() },
        report: None,
    };
    let mut preset: Option<String> = None;
    let mut mask = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--folder" => cli.request.folder = arg_value(args, &mut i, "--folder")?,
            "--preset" => preset = Some(arg_value(args, &mut i, "--preset")?),
            "--report" => cli.report = Some(arg_value(args, &mut i, "--report")?),
            "--mask" => mask = arg_value(args, &mut i, "--mask")?,
            "--dry" => cli.request.dry = true,
            "--no-backup" => cli.request.backup = false,
            "--flat" => cli.request.recursive = false,
            other => return Err(format!("err.cli_usage\u{1f}{other}")),
        }
        i += 1;
    }
    let Some(p) = preset else {
        return Err("err.cli_usage\u{1f}--preset".into());
    };
    if cli.request.folder.is_empty() {
        return Err("err.cli_usage\u{1f}--folder".into());
    }
    let tpl = load_template(&p)?;
    cli.request.rules = tpl.rules;
    cli.request.mask = if mask.is_empty() { tpl.mask.unwrap_or_default() } else { mask };
    if cli.request.mask.trim().is_empty() {
        cli.request.mask = "*".into();
    }
    if let Some(v) = tpl.options.keep_symbols {
        cli.request.keep_symbols = v;
    }
    if let Some(v) = tpl.options.scripts {
        cli.request.scripts = v;
    }
    if let Some(v) = tpl.options.min_size {
        cli.request.min_size = v;
    }
    Ok(Some(cli))
}

/// Runs a command line request. `None` = no arguments, the window should open.
/// Exit code: 0 = done, 1 = some files failed.
pub fn run_cli(args: &[String]) -> Result<Option<i32>, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", cli_usage());
        return Ok(Some(0));
    }
    let Some(cli) = parse_cli(args)? else {
        return Ok(None);
    };
    let events = Mutex::new(Vec::new());
    let stats = run(&cli.request, |ev| {
        if let Ok(mut g) = events.lock() {
            g.push(ev);
        }
    })?;
    let events = events.into_inner().unwrap_or_default();
    if let Some(path) = &cli.report {
        let rows: Vec<ReportRow> = events
            .iter()
            .map(|e| ReportRow {
                path: e.path.clone(),
                count: if e.count > 0 { e.count.to_string() } else { String::new() },
                status: if e.info.is_empty() { e.status.clone() } else { format!("{}: {}", e.status, e.info) },
            })
            .collect();
        write_report(path, &rows)?;
    }
    println!(
        "files: {}, changed: {}, fonts: {}, sizes: {}, skipped: {}, errors: {}",
        stats.files, stats.changed, stats.fonts, stats.sizes, stats.skipped, stats.errors
    );
    Ok(Some(if stats.errors > 0 { 1 } else { 0 }))
}

// ───────────── tests ─────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(ff: &str, tf: &str, fs: &str, ts: &str) -> Rule {
        Rule {
            from_font: ff.into(),
            to_font: tf.into(),
            from_size: fs.into(),
            to_size: ts.into(),
            ..Default::default()
        }
    }

    fn req(rules: Vec<Rule>) -> Request {
        Request { mask: "*".into(), rules, recursive: true, keep_symbols: true, ..Default::default() }
    }

    #[test]
    fn glob_masks() {
        let g = |m: &str, n: &str| {
            let masks = parse_masks(m);
            let name: Vec<char> = n.to_lowercase().chars().collect();
            masks.iter().any(|p| glob(p, &name))
        };
        assert!(g("*", "a.docx"));
        assert!(g("*.docx;*.xlsx", "Report.XLSX"));
        assert!(!g("*.docx", "a.xlsx"));
        assert!(g("report?.docx", "report1.docx"));
    }

    #[test]
    fn sizes_are_parsed_leniently() {
        assert_eq!(parse_size(""), Ok(None));
        assert_eq!(parse_size(" 12 "), Ok(Some(12.0)));
        assert_eq!(parse_size("10,5"), Ok(Some(10.5)));
        assert_eq!(parse_size("11 pt"), Ok(Some(11.0)));
        assert!(parse_size("abc").is_err());
        assert!(parse_size("0").is_err());
        assert!(parse_size("5000").is_err());
        assert!(parse_size("nan").is_err());
    }

    #[test]
    fn compile_validates_rules() {
        let o = req(vec![]);
        assert_eq!(compile(&[], &o).unwrap_err(), "err.no_rules");
        assert_eq!(compile(&[rule("", "", "", "")], &o).unwrap_err(), "err.rule_empty\u{1f}1");
        assert_eq!(compile(&[rule("Arial", "", "", "12")], &o).unwrap_err(), "err.rule_font_target\u{1f}1");
        assert_eq!(compile(&[rule("", "Arial", "12", "")], &o).unwrap_err(), "err.rule_size_target\u{1f}1");
        assert_eq!(compile(&[rule("", "Ari;al", "", "")], &o).unwrap_err(), "err.font_name\u{1f}1");
        assert_eq!(compile(&[rule("", "Arial", "", ""), rule("", "", "", "x")], &o).unwrap_err(), "err.size\u{1f}2");
    }

    #[test]
    fn compile_puts_specific_rules_first() {
        let o = req(vec![]);
        let r = compile(&[rule("", "Verdana", "", "14"), rule("Arial", "Calibri", "12", "11")], &o).unwrap();
        assert_eq!(r.fonts[0].from.as_deref(), Some("arial"));
        assert!(r.fonts[1].from.is_none());
        assert_eq!(r.sizes[0].from, Some(12.0));
        assert!(r.sizes[1].from.is_none());
    }

    #[test]
    fn file_kinds() {
        assert!(matches!(file_kind(Path::new("a.DOCX")), FileKind::Zip(Kind::Word)));
        assert!(matches!(file_kind(Path::new("a.odp")), FileKind::Zip(Kind::Odf)));
        assert!(matches!(file_kind(Path::new("a.doc")), FileKind::Legacy));
        assert!(matches!(file_kind(Path::new("a.pdf")), FileKind::Pdf));
        assert!(matches!(file_kind(Path::new("a.txt")), FileKind::Other));
        assert!(matches!(file_kind(Path::new("noext")), FileKind::Other));
    }

    fn write_zip(path: &Path, parts: &[(&str, &str)]) {
        let f = fs::File::create(path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, body) in parts {
            z.start_file(*name, opts).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn run_changes_fonts_inside_charts() {
        let dir = std::env::temp_dir().join(format!("fc-chart-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("a.docx");
        write_zip(
            &doc,
            &[
                ("[Content_Types].xml", "<Types/>"),
                (
                    "word/document.xml",
                    r#"<w:document><w:body><w:p><w:r><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/></w:rPr><w:t>x</w:t></w:r></w:p></w:body></w:document>"#,
                ),
                (
                    "word/charts/chart1.xml",
                    r#"<c:chartSpace><c:txPr><a:defRPr><a:latin typeface="Arial"/></a:defRPr></c:txPr></c:chartSpace>"#,
                ),
            ],
        );
        let o = Request {
            folder: dir.to_string_lossy().into(),
            mask: "*.docx".into(),
            rules: vec![rule("Arial", "Calibri", "", "")],
            recursive: true,
            backup: false,
            keep_symbols: true,
            ..Default::default()
        };
        let stats = run(&o, |_| {}).unwrap();
        assert_eq!(stats.changed, 1);
        // ascii + hAnsi in the body, plus the typeface inside the chart
        assert_eq!(stats.fonts, 3, "body and chart must both be rewritten");

        let mut z = zip::ZipArchive::new(fs::File::open(&doc).unwrap()).unwrap();
        let mut chart = String::new();
        z.by_name("word/charts/chart1.xml").unwrap().read_to_string(&mut chart).unwrap();
        assert!(chart.contains(r#"typeface="Calibri""#), "chart kept the old font: {chart}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_lists_leftovers_and_is_empty_after_run() {
        let dir = std::env::temp_dir().join(format!("fc-verify-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        write_zip(
            &dir.join("a.docx"),
            &[
                ("[Content_Types].xml", "<Types/>"),
                (
                    "word/document.xml",
                    r#"<w:document><w:body><w:p><w:r><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/></w:rPr><w:t>x</w:t></w:r></w:p></w:body></w:document>"#,
                ),
            ],
        );
        let o = Request {
            folder: dir.to_string_lossy().into(),
            mask: "*.docx".into(),
            rules: vec![rule("Arial", "Calibri", "", "")],
            recursive: true,
            backup: false,
            keep_symbols: true,
            ..Default::default()
        };
        let left = verify(&o).unwrap();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(left[0].fonts, vec!["Arial".to_string()]);
        run(&o, |_| {}).unwrap();
        assert!(verify(&o).unwrap().is_empty(), "nothing must be left after a successful run");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sample_picks_the_most_used_font_and_size() {
        let dir = std::env::temp_dir().join(format!("fc-sample-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("sample.docx");
        let para = |font: &str, sz: &str, text: &str| {
            format!(
                r#"<w:p><w:r><w:rPr><w:rFonts w:ascii="{font}" w:hAnsi="{font}"/><w:sz w:val="{sz}"/></w:rPr><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        let body = format!(
            "<w:document><w:body>{}{}{}</w:body></w:document>",
            para("Arial", "24", "a"),
            para("Arial", "24", "b"),
            para("Calibri", "28", "c")
        );
        write_zip(&doc, &[("[Content_Types].xml", "<Types/>"), ("word/document.xml", body.as_str())]);

        let s = sample(&doc.to_string_lossy()).unwrap();
        assert_eq!(s.fonts[0].0, "Arial", "{s:?}");
        assert!(s.fonts[0].1 > s.fonts[1].1, "{s:?}");
        assert_eq!(s.sizes[0].0, 12.0, "{s:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_is_written_as_csv_with_bom() {
        let dir = std::env::temp_dir().join(format!("fc-report-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("report.csv");
        let rows = vec![ReportRow {
            path: "a,\"b\".docx".into(),
            count: "2".into(),
            status: "changed: fonts 2, sizes 0".into(),
        }];
        write_report(&p.to_string_lossy(), &rows).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.starts_with('\u{feff}'), "BOM keeps Cyrillic readable in Excel");
        assert!(text.contains("\"a,\"\"b\"\".docx\""), "{text}");
        assert!(text.contains("\"2\""));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scope_limits_a_rule_to_its_parts() {
        let dir = std::env::temp_dir().join(format!("fc-scope-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("a.docx");
        let run_text = |text: &str| {
            format!(
                r#"<w:p><w:r><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/></w:rPr><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        let body = format!("<w:document><w:body>{}</w:body></w:document>", run_text("body"));
        let head = format!("<w:hdr>{}</w:hdr>", run_text("head"));
        write_zip(
            &doc,
            &[
                ("[Content_Types].xml", "<Types/>"),
                ("word/document.xml", body.as_str()),
                ("word/header1.xml", head.as_str()),
            ],
        );
        let o = Request {
            folder: dir.to_string_lossy().into(),
            mask: "*.docx".into(),
            rules: vec![Rule {
                from_font: "Arial".into(),
                to_font: "Calibri".into(),
                scope: "headers".into(),
                ..Default::default()
            }],
            recursive: true,
            backup: false,
            keep_symbols: true,
            ..Default::default()
        };
        run(&o, |_| {}).unwrap();

        let mut z = zip::ZipArchive::new(fs::File::open(&doc).unwrap()).unwrap();
        let mut body_xml = String::new();
        z.by_name("word/document.xml").unwrap().read_to_string(&mut body_xml).unwrap();
        let mut head_xml = String::new();
        z.by_name("word/header1.xml").unwrap().read_to_string(&mut head_xml).unwrap();
        assert!(body_xml.contains("Arial"), "the body must stay as it is: {body_xml}");
        assert!(head_xml.contains("Calibri"), "the header must change: {head_xml}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_scope_is_rejected() {
        let o = req(vec![]);
        let r = Rule {
            from_font: "Arial".into(),
            to_font: "Calibri".into(),
            scope: "sidebar".into(),
            ..Default::default()
        };
        assert_eq!(compile(&[r], &o).unwrap_err(), "err.scope\u{1f}1");
    }

    #[test]
    fn template_v2_keeps_the_scope() {
        let dir = std::env::temp_dir().join(format!("fc-tpl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.fctpl");
        let tpl = Template {
            rules: vec![Rule {
                from_font: "Arial".into(),
                to_font: "Calibri".into(),
                scope: "footers".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let written = save_template(&p.to_string_lossy(), tpl).unwrap();
        let back = load_template(&written).unwrap();
        assert_eq!(back.version, 2);
        assert_eq!(back.rules[0].scope, "footers");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_run_changes_files_and_writes_a_report() {
        let dir = std::env::temp_dir().join(format!("fc-cli-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("a.docx");
        write_zip(
            &doc,
            &[
                ("[Content_Types].xml", "<Types/>"),
                (
                    "word/document.xml",
                    r#"<w:document><w:body><w:p><w:r><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/></w:rPr><w:t>x</w:t></w:r></w:p></w:body></w:document>"#,
                ),
            ],
        );
        let preset = dir.join("p.fctpl");
        save_template(
            &preset.to_string_lossy(),
            Template { rules: vec![rule("Arial", "Calibri", "", "")], ..Default::default() },
        )
        .unwrap();
        let report = dir.join("r.csv");
        let args: Vec<String> = vec![
            "--folder".into(),
            dir.to_string_lossy().to_string(),
            "--preset".into(),
            preset.to_string_lossy().to_string(),
            "--report".into(),
            report.to_string_lossy().to_string(),
            "--mask".into(),
            "*.docx".into(),
        ];
        assert_eq!(run_cli(&args).unwrap(), Some(0));

        let text = fs::read_to_string(&report).unwrap();
        assert!(text.contains("ok") && text.contains("a.docx"), "{text}");
        let mut z = zip::ZipArchive::new(fs::File::open(&doc).unwrap()).unwrap();
        let mut xml = String::new();
        z.by_name("word/document.xml").unwrap().read_to_string(&mut xml).unwrap();
        assert!(xml.contains("Calibri"), "{xml}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_fonts_are_the_ones_not_installed() {
        let found = vec![
            FontInfo { name: "Arial".into(), files: 3, ..Default::default() },
            FontInfo { name: "Calibri".into(), files: 1, ..Default::default() },
            FontInfo { name: "Wingdings".into(), files: 2, symbol: true, ..Default::default() },
            FontInfo { name: "inherit".into(), files: 1, invalid: true, ..Default::default() },
        ];
        let installed: HashSet<String> = ["arial".to_string()].into_iter().collect();
        let list = missing(&found, &installed);
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].name, "Calibri");
    }

    #[test]
    fn installed_families_are_lower_case() {
        for name in installed_families() {
            assert_eq!(name, name.to_lowercase(), "{name}");
        }
    }

    #[test]
    fn style_rule_touches_only_that_style() {
        let dir = std::env::temp_dir().join(format!("fc-style-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("a.docx");
        let run_text = |text: &str| {
            format!(
                r#"<w:r><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/></w:rPr><w:t>{text}</w:t></w:r>"#
            )
        };
        let body = format!(
            "<w:document><w:body><w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr>{}</w:p><w:p>{}</w:p></w:body></w:document>",
            run_text("head"),
            run_text("body")
        );
        let styles = r#"<w:styles><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style></w:styles>"#;
        write_zip(
            &doc,
            &[
                ("[Content_Types].xml", "<Types/>"),
                ("word/document.xml", body.as_str()),
                ("word/styles.xml", styles),
            ],
        );
        let o = Request {
            folder: dir.to_string_lossy().into(),
            mask: "*.docx".into(),
            rules: vec![Rule {
                from_font: "Arial".into(),
                to_font: "Calibri".into(),
                style: "heading 1".into(),
                ..Default::default()
            }],
            recursive: true,
            backup: false,
            keep_symbols: true,
            ..Default::default()
        };
        run(&o, |_| {}).unwrap();

        let mut z = zip::ZipArchive::new(fs::File::open(&doc).unwrap()).unwrap();
        let mut xml = String::new();
        z.by_name("word/document.xml").unwrap().read_to_string(&mut xml).unwrap();
        assert_eq!(xml.matches("Calibri").count(), 2, "the heading must change: {xml}");
        assert_eq!(xml.matches("Arial").count(), 2, "the body must stay: {xml}");
        let _ = fs::remove_dir_all(&dir);
    }

    fn two_part_docx(path: &Path, body_paragraphs: &str, header: &str) {
        let body = format!("<w:document><w:body>{body_paragraphs}</w:body></w:document>");
        let head = format!("<w:hdr>{header}</w:hdr>");
        write_zip(
            path,
            &[("[Content_Types].xml", "<Types/>"), ("word/document.xml", body.as_str()), ("word/header1.xml", head.as_str())],
        );
    }

    fn read_part(path: &Path, name: &str) -> String {
        let mut z = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        let mut s = String::new();
        z.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    fn calibri_run(text: &str) -> String {
        format!(
            r#"<w:r><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri"/><w:sz w:val="40"/></w:rPr><w:t>{text}</w:t></w:r>"#
        )
    }

    #[test]
    fn area_rule_beats_the_general_rule() {
        let dir = temp_dir("area-priority");
        let doc = dir.join("a.docx");
        two_part_docx(&doc, &format!("<w:p>{}</w:p>", calibri_run("body")), &format!("<w:p>{}</w:p>", calibri_run("head")));
        let mut o = req(vec![
            rule("", "Times New Roman", "", ""),
            Rule { to_font: "Arial".into(), scope: "headers".into(), ..Default::default() },
        ]);
        o.folder = dir.to_string_lossy().to_string();
        run(&o, |_| {}).unwrap();
        let body = read_part(&doc, "word/document.xml");
        let head = read_part(&doc, "word/header1.xml");
        assert_eq!(body.matches("Times New Roman").count(), 2, "{body}");
        assert_eq!(head.matches("Arial").count(), 2, "{head}");
        assert!(!head.contains("Times New Roman"), "{head}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn style_rule_beats_the_general_rule_and_nothing_is_counted_twice() {
        let dir = temp_dir("style-priority");
        let doc = dir.join("a.docx");
        let heading = format!(r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>{}</w:p>"#, calibri_run("head"));
        let plain = format!("<w:p>{}</w:p>", calibri_run("body"));
        two_part_docx(&doc, &format!("{heading}{plain}"), "");
        let mut o = req(vec![
            rule("", "Times New Roman", "", "14"),
            Rule { to_font: "Arial".into(), to_size: "16".into(), style: "Heading1".into(), ..Default::default() },
        ]);
        o.folder = dir.to_string_lossy().to_string();
        let stats = run(&o, |_| {}).unwrap();
        let body = read_part(&doc, "word/document.xml");
        assert_eq!(body.matches("Arial").count(), 2, "heading: {body}");
        assert_eq!(body.matches("Times New Roman").count(), 2, "body: {body}");
        assert_eq!(body.matches(r#"w:val="32""#).count(), 1, "{body}");
        assert_eq!(body.matches(r#"w:val="28""#).count(), 1, "{body}");
        assert_eq!((stats.fonts, stats.sizes), (4, 2), "every change counted once");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_style_rule_beats_explicit_general_rule() {
        let dir = temp_dir("style-explicit");
        let doc = dir.join("a.docx");
        let arial = |t: &str| {
            format!(r#"<w:r><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/></w:rPr><w:t>{t}</w:t></w:r>"#)
        };
        let heading = format!(r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>{}</w:p>"#, arial("head"));
        let plain = format!("<w:p>{}</w:p>", arial("body"));
        two_part_docx(&doc, &format!("{heading}{plain}"), "");
        let mut o = req(vec![
            rule("Arial", "Verdana", "", ""),
            Rule { from_font: "Arial".into(), to_font: "Calibri".into(), style: "Heading1".into(), ..Default::default() },
        ]);
        o.folder = dir.to_string_lossy().to_string();
        run(&o, |_| {}).unwrap();
        let body = read_part(&doc, "word/document.xml");
        assert_eq!(body.matches("Calibri").count(), 2, "heading: {body}");
        assert_eq!(body.matches("Verdana").count(), 2, "body: {body}");
        assert!(!body.contains("Arial"), "{body}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn normal_style_rule_reaches_paragraphs_without_a_style() {
        let dir = temp_dir("style-normal");
        let doc = dir.join("a.docx");
        let arial = |t: &str| {
            format!(r#"<w:r><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial"/></w:rPr><w:t>{t}</w:t></w:r>"#)
        };
        let heading = format!(r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>{}</w:p>"#, arial("head"));
        let plain = format!("<w:p>{}</w:p>", arial("body"));
        let styles = concat!(
            r#"<w:styles><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>"#,
            r#"<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style></w:styles>"#
        );
        let body_xml = format!("<w:document><w:body>{heading}{plain}</w:body></w:document>");
        write_zip(
            &doc,
            &[("[Content_Types].xml", "<Types/>"), ("word/document.xml", body_xml.as_str()), ("word/styles.xml", styles)],
        );
        let mut o = req(vec![Rule {
            from_font: "Arial".into(),
            to_font: "Calibri".into(),
            style: "Normal".into(),
            ..Default::default()
        }]);
        o.folder = dir.to_string_lossy().to_string();
        run(&o, |_| {}).unwrap();
        let body = read_part(&doc, "word/document.xml");
        assert_eq!(body.matches("Calibri").count(), 2, "the paragraph without a style is Normal: {body}");
        assert_eq!(body.matches("Arial").count(), 2, "the heading must stay: {body}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_known_arguments_start_the_command_line_mode() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        assert!(!wants_cli(&args(&[])));
        assert!(!wants_cli(&args(&["-psn_0_12345"])), "a launcher argument must not block the window");
        assert!(!wants_cli(&args(&["/some/file.docx"])));
        assert!(wants_cli(&args(&["--folder", "/docs"])));
        assert!(wants_cli(&args(&["--preset", "a.fctpl"])));
        assert!(wants_cli(&args(&["--help"])));
    }

    #[test]
    fn command_line_errors_are_readable() {
        let m = cli_message("err.cli_usage\u{1f}--bogus");
        assert!(m.contains("--bogus") && m.contains("--folder"), "{m}");
        assert!(cli_message("err.cli_value\u{1f}--mask").contains("--mask needs a value"));
        assert_eq!(cli_message("err.scope\u{1f}2"), "Rule 2: unknown area");
        assert_eq!(cli_message("something else"), "something else");
    }

    #[test]
    fn routing_charts_and_smartart() {
        // charts and SmartArt used to be skipped silently; they are DrawingML text, like slides
        assert_eq!(route(Kind::Word, "word/charts/chart1.xml"), Some(Route::DrawingMl));
        assert_eq!(route(Kind::Word, "word/diagrams/data1.xml"), Some(Route::DrawingMl));
        assert_eq!(route(Kind::Word, "word/diagrams/drawing1.xml"), Some(Route::DrawingMl));
        assert_eq!(route(Kind::Excel, "xl/charts/chart1.xml"), Some(Route::DrawingMl));
        assert_eq!(route(Kind::Slides, "ppt/charts/chart1.xml"), Some(Route::DrawingMl));
        // parts that hold no fonts, relations and embedded objects stay untouched
        assert_eq!(route(Kind::Word, "word/charts/colors1.xml"), None);
        assert_eq!(route(Kind::Word, "word/charts/_rels/chart1.xml.rels"), None);
        assert_eq!(route(Kind::Excel, "xl/charts/style1.xml"), None);
        assert_eq!(route(Kind::Word, "word/embeddings/Worksheet1.xlsx"), None);
        assert_eq!(route(Kind::Word, "word/media/image1.png"), None);
    }

    #[test]
    fn routing() {
        assert_eq!(route(Kind::Word, "word/document.xml"), Some(Route::Word));
        assert_eq!(route(Kind::Word, "word/header2.xml"), Some(Route::Word));
        assert_eq!(route(Kind::Word, "word/theme/theme1.xml"), Some(Route::Theme));
        assert_eq!(route(Kind::Word, "word/media/image1.png"), None);
        assert_eq!(route(Kind::Excel, "xl/styles.xml"), Some(Route::ExcelStyles));
        assert_eq!(route(Kind::Excel, "xl/worksheets/sheet1.xml"), Some(Route::ExcelSheet));
        assert_eq!(route(Kind::Excel, "xl/worksheets/_rels/sheet1.xml.rels"), None);
        assert_eq!(route(Kind::Slides, "ppt/slides/slide1.xml"), Some(Route::DrawingMl));
        assert_eq!(route(Kind::Slides, "ppt/slideMasters/slideMaster1.xml"), Some(Route::DrawingStyles));
        assert_eq!(route(Kind::Slides, "ppt/presentation.xml"), Some(Route::DrawingStyles));
        assert_eq!(route(Kind::Slides, "ppt/slides/_rels/slide1.xml.rels"), None);
        assert_eq!(route(Kind::Slides, "ppt/diagrams/data1.xml"), Some(Route::DrawingMl));
        assert_eq!(route(Kind::Slides, "ppt/diagrams/colors1.xml"), None);
        assert_eq!(route(Kind::Odf, "content.xml"), Some(Route::Odf));
        assert_eq!(route(Kind::Odf, "META-INF/manifest.xml"), None);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fontchanger-test-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_zip(path: &Path, files: &[(&str, &str)]) {
        let mut zw = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (name, data) in files {
            zw.start_file(*name, SimpleFileOptions::default()).unwrap();
            zw.write_all(data.as_bytes()).unwrap();
        }
        zw.finish().unwrap();
    }

    fn read_zip_entry(path: &Path, name: &str) -> String {
        let mut ar = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        let mut f = ar.by_name(name).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn docx_round_trip_with_backup_and_dry_run() {
        let dir = temp_dir("docx");
        let file = dir.join("a.docx");
        let doc = r#"<w:document><w:body><w:p><w:r><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri"/><w:sz w:val="22"/></w:rPr><w:t>Hi</w:t></w:r></w:p></w:body></w:document>"#;
        make_zip(&file, &[("[Content_Types].xml", "<Types/>"), ("word/document.xml", doc)]);

        // dry run changes nothing
        let mut o = req(vec![rule("", "Arial", "", "14")]);
        o.folder = dir.to_string_lossy().to_string();
        o.dry = true;
        let stats = run(&o, |_| {}).unwrap();
        assert_eq!(stats.changed, 1);
        assert_eq!((stats.fonts, stats.sizes), (2, 1));
        assert!(read_zip_entry(&file, "word/document.xml").contains("Calibri"));

        // real run
        o.dry = false;
        o.backup = true;
        let stats = run(&o, |_| {}).unwrap();
        assert_eq!(stats.changed, 1);
        let xml = read_zip_entry(&file, "word/document.xml");
        assert!(xml.contains(r#"<w:rFonts w:ascii="Arial" w:hAnsi="Arial"/>"#));
        assert!(xml.contains(r#"<w:sz w:val="28"/>"#));
        assert_eq!(read_zip_entry(&file, "[Content_Types].xml"), "<Types/>");
        assert!(dir.join("a.docx.bak").exists());

        // running again finds nothing to do
        let stats = run(&o, |_| {}).unwrap();
        assert_eq!(stats.changed, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mixed_folder_rtf_html_and_skips() {
        let dir = temp_dir("mixed");
        fs::write(dir.join("a.rtf"), br"{\rtf1{\fonttbl{\f0\fswiss Arial;}}\f0\fs24 Hi}").unwrap();
        fs::write(dir.join("a.css"), "p { font-family: Arial; font-size: 12pt }").unwrap();
        fs::write(dir.join("notes.txt"), "Arial 12").unwrap();
        fs::write(dir.join("old.doc"), "x").unwrap();
        fs::write(dir.join("x.pdf"), "x").unwrap();

        let mut o = req(vec![rule("Arial", "Georgia", "12", "11")]);
        o.folder = dir.to_string_lossy().to_string();
        let stats = run(&o, |_| {}).unwrap();
        assert_eq!(stats.files, 4); // txt is ignored, doc and pdf are reported
        assert_eq!(stats.changed, 2);
        assert_eq!(stats.skipped, 2);
        assert_eq!(stats.errors, 0);

        let rtf = fs::read_to_string(dir.join("a.rtf")).unwrap();
        assert!(rtf.contains("Georgia;") && rtf.contains(r"\fs22"));
        let css = fs::read_to_string(dir.join("a.css")).unwrap();
        assert_eq!(css, "p { font-family: Georgia; font-size: 11pt }");
        assert_eq!(fs::read_to_string(dir.join("notes.txt")).unwrap(), "Arial 12");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_labels_where_sizes_are_and_flags_invalid_fonts() {
        let dir = temp_dir("scanctx");
        let doc = r#"<w:p><w:pPr><w:rPr><w:sz w:val="2"/></w:rPr></w:pPr><w:r><w:rPr><w:rFonts w:ascii="inherit" w:hAnsi="inherit"/><w:sz w:val="24"/></w:rPr><w:t>Hello</w:t></w:r></w:p>"#;
        let styles = r#"<w:styles><w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="22"/></w:rPr></w:rPrDefault></w:docDefaults></w:styles>"#;
        make_zip(&dir.join("a.docx"), &[("word/document.xml", doc), ("word/styles.xml", styles)]);
        let mut o = req(vec![]);
        o.folder = dir.to_string_lossy().to_string();
        let res = scan(&o, || false).unwrap();
        let by = |pt: f64| res.sizes.iter().find(|s| (s.size - pt).abs() < 0.001).unwrap();
        assert_eq!((by(12.0).text, by(12.0).style, by(12.0).hidden), (1, 0, 0));
        assert_eq!((by(11.0).text, by(11.0).style, by(11.0).hidden), (0, 1, 0));
        assert_eq!((by(1.0).text, by(1.0).style, by(1.0).hidden), (0, 0, 1));
        assert!(res.fonts.iter().find(|f| f.name == "inherit").unwrap().invalid);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn minimum_size_protects_tiny_sizes_in_a_real_run() {
        let dir = temp_dir("minsize");
        let doc = r#"<w:p><w:pPr><w:rPr><w:sz w:val="2"/></w:rPr></w:pPr><w:r><w:rPr><w:sz w:val="16"/></w:rPr><w:t>Hi</w:t></w:r></w:p>"#;
        make_zip(&dir.join("a.docx"), &[("word/document.xml", doc)]);
        let mut o = req(vec![rule("", "", "", "12")]);
        o.folder = dir.to_string_lossy().to_string();
        o.min_size = 6.0;
        let stats = run(&o, |_| {}).unwrap();
        assert_eq!(stats.sizes, 1);
        let xml = read_zip_entry(&dir.join("a.docx"), "word/document.xml");
        assert!(xml.contains(r#"<w:sz w:val="2"/>"#) && xml.contains(r#"<w:sz w:val="24"/>"#));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tab_separated_rules_file_is_still_loaded() {
        let dir = temp_dir("rules");
        let f = dir.join("rules.txt");
        fs::write(&f, "# comment\nArial\tCalibri\t\t\n\tTimes New Roman\t\t14\n\n").unwrap();
        let tpl = load_template(&f.to_string_lossy()).unwrap();
        assert_eq!(tpl.rules.len(), 2);
        assert_eq!((tpl.rules[0].from_font.as_str(), tpl.rules[0].to_font.as_str()), ("Arial", "Calibri"));
        assert_eq!((tpl.rules[1].from_font.as_str(), tpl.rules[1].to_size.as_str()), ("", "14"));
        fs::write(&f, "# nothing\n").unwrap();
        assert_eq!(load_template(&f.to_string_lossy()).unwrap_err(), "err.no_rules_in_file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn template_round_trip() {
        let dir = temp_dir("tpl");
        let target = dir.join("my rules"); // no extension: .fctpl is added
        let tpl = Template {
            mask: Some("*.docx".into()),
            options: TemplateOptions { keep_symbols: Some(true), min_size: Some(6.0), ..Default::default() },
            rules: vec![rule("Arial", "Calibri", "", ""), rule("", "Times New Roman", "", "14")],
            ..Default::default()
        };
        let written = save_template(&target.to_string_lossy(), tpl).unwrap();
        assert!(written.ends_with("my rules.fctpl"));
        let text = fs::read_to_string(&written).unwrap();
        assert!(text.contains("\"format\": \"fctpl\"") && text.contains("\"version\": 2"));
        assert!(!text.contains("scripts")); // unset options are not written

        let back = load_template(&written).unwrap();
        assert_eq!(back.name, "my rules");
        assert_eq!(back.mask.as_deref(), Some("*.docx"));
        assert_eq!(back.options.keep_symbols, Some(true));
        assert_eq!(back.options.scripts, None);
        assert_eq!(back.options.min_size, Some(6.0));
        assert_eq!(back.rules.len(), 2);
        assert_eq!(back.rules[1].to_size, "14");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn template_errors() {
        let dir = temp_dir("tplerr");
        let f = dir.join("x.fctpl");
        fs::write(&f, r#"{"format":"other","version":1,"rules":[{"toFont":"Arial"}]}"#).unwrap();
        assert_eq!(load_template(&f.to_string_lossy()).unwrap_err(), "err.template_format");
        fs::write(&f, r#"{"format":"fctpl","version":99,"rules":[{"toFont":"Arial"}]}"#).unwrap();
        assert_eq!(load_template(&f.to_string_lossy()).unwrap_err(), "err.template_version\u{1f}99");
        fs::write(&f, r#"{"format":"fctpl","version":1,"rules":[]}"#).unwrap();
        assert_eq!(load_template(&f.to_string_lossy()).unwrap_err(), "err.no_rules_in_file");
        fs::write(&f, "{ not json").unwrap();
        assert!(load_template(&f.to_string_lossy()).unwrap_err().starts_with("err.template_parse\u{1f}"));
        // rules that cannot run are not saved
        let bad = Template { rules: vec![rule("Arial", "", "", "12")], ..Default::default() };
        assert_eq!(save_template(&f.to_string_lossy(), bad).unwrap_err(), "err.rule_font_target\u{1f}1");
        // unknown fields are ignored
        fs::write(&f, r#"{"format":"fctpl","version":1,"future":true,"rules":[{"toFont":"Arial","extra":1}]}"#).unwrap();
        assert_eq!(load_template(&f.to_string_lossy()).unwrap().rules[0].to_font, "Arial");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_lists_fonts_and_sizes_and_changes_nothing() {
        let dir = temp_dir("scan");
        let a = r#"<w:rFonts w:ascii="Arial" w:hAnsi="Arial"/><w:rFonts w:ascii="Wingdings"/><w:sz w:val="24"/>"#;
        let b = r#"<w:rFonts w:ascii="Arial"/><w:rFonts w:ascii="Calibri"/><w:sz w:val="24"/><w:sz w:val="21"/>"#;
        make_zip(&dir.join("a.docx"), &[("word/document.xml", a)]);
        make_zip(&dir.join("b.docx"), &[("word/document.xml", b)]);
        fs::write(dir.join("c.css"), "p { font-family: Verdana; font-size: 12pt }").unwrap();
        fs::write(dir.join("t.txt"), "Arial").unwrap();
        let before = fs::read(dir.join("a.docx")).unwrap();

        let mut o = req(vec![]);
        o.folder = dir.to_string_lossy().to_string();
        let res = scan(&o, || false).unwrap();
        assert_eq!(res.files, 3);
        let names: Vec<&str> = res.fonts.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names[0], "Arial"); // used in two files: first
        assert!(names.contains(&"Calibri") && names.contains(&"Verdana") && names.contains(&"Wingdings"));
        assert_eq!(res.fonts[0].files, 2);
        assert!(res.fonts.iter().find(|f| f.name == "Wingdings").unwrap().symbol);
        let sizes: Vec<f64> = res.sizes.iter().map(|s| s.size).collect();
        assert_eq!(sizes, vec![10.5, 12.0]);
        assert_eq!(res.sizes[1].files, 3); // 12 pt is in all three files
        // CSS does not say where a size sits, so it counts as text; the bare XML above is outside any run
        assert_eq!((res.sizes[1].text, res.sizes[1].hidden), (1, 2));
        assert_eq!(fs::read(dir.join("a.docx")).unwrap(), before);

        // a single mask narrows the scan
        o.mask = "c.css".into();
        let one = scan(&o, || false).unwrap();
        assert_eq!((one.files, one.fonts.len(), one.sizes.len()), (1, 1, 1));

        // cancellation
        assert_eq!(scan(&o, || true).unwrap_err(), "cancelled");
        assert_eq!(
            scan(&Request { folder: "/definitely/not/here".into(), ..Default::default() }, || false).unwrap_err(),
            "err.folder_not_found"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
