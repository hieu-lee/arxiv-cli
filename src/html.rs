//! arXiv LaTeXML HTML (arxiv.org/html, ar5iv) -> Markdown + structure, arXiv abs-page metadata, grep.
//!
//! One hand-rolled single-pass tokenizer (`Parser`) drives everything: it keeps a stack of open
//! elements, each carrying an `Act` decided from the tag name and `ltx_*` classes at open time,
//! and writes Markdown into one `String` while recording sections, theorem-like items,
//! bibliography entries and author institutions on the way. MathML bodies are skipped wholesale
//! (the TeX comes from `alttext`).

use crate::model::*;
use std::borrow::Cow;
use std::cell::Cell;

/// What the caller reads from the result. Everything else is not built (cheaper), and outside
/// `All` / `Text` the `md` and the section byte ranges only cover the parts that were needed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Want {
    /// every field (tests; the commands name what they read)
    #[allow(dead_code)]
    All,
    /// `title`, `md`, `sections` (text, sec, grep)
    Text,
    /// `title`, `sections` ids / levels / titles (outline)
    Outline,
    /// `title`, `items` (prob)
    Items,
    /// `title`, `refs`
    Refs,
}

pub struct Doc {
    pub title: String,
    /// Whole paper as Markdown ($latex$ math from alttext).
    pub md: String,
    pub sections: Vec<Section>,
    pub items: Vec<Item>,
    pub refs: Vec<Ref>,
}

#[cfg(test)]
pub fn parse(html: &str) -> Doc {
    parse_for(html, Want::All)
}

/// Parse an arxiv.org/html (LaTeXML) or ar5iv page. Never panics on junk.
///
/// Markdown conventions: `# title`, sections `## 2 Heading` (section level 1), `### 2.1 ..`,
/// paragraphs separated by a blank line, lists `- `, table rows `a | b`, theorem blocks
/// `**Problem 1.** text`, inline math `$..$`, display math `$$..$$`, captions `[fig: ..]`
/// (`[table: ..]`, `[listing: ..]`), footnotes `[fn: ..]`.
/// `Section.level` is 1 for \section, 2 for \subsection, 3 for \subsubsection.
///
/// Only what `want` names is built.
pub fn parse_for(html: &str, want: Want) -> Doc {
    let start = find_article(html.as_bytes()).unwrap_or(0);
    // the Markdown of a paper is typically 0.3-0.4 of its article markup
    let md_cap = if matches!(want, Want::All | Want::Text) { (html.len() - start) / 5 * 2 } else { 1024 };
    let mut p = Parser::new(html, want, md_cap);
    p.i = start;
    p.run();
    let fallback_title = if start > 0 { head_title(&html[..start]) } else { head_title(html) };
    p.finish(fallback_title)
}

/// Only the institutions, scanning just the head of the page (fast path for --inst).
/// The author block is located inside the first 256KB (arXiv pages carry ~30-60KB of inline
/// CSS/JS before it) and only that block is tokenised.
pub fn insts_only(html: &str) -> Vec<String> {
    let head = floor_boundary(html, html.len().min(256 * 1024));
    let Some(k) = find_pat(&html.as_bytes()[..head], 0, b"ltx_authors", &AUTHORS) else { return Vec::new() };
    let Some(lt) = html[..k].rfind('<') else { return Vec::new() };
    let mut p = Parser::new(html, Want::Outline, 0);
    p.i = lt;
    p.stop_after_authors = true;
    p.run();
    p.insts
}

pub struct AbsMeta {
    pub msc: Vec<String>,
    /// (version label "v2", date "2024-03-05", size "12kb")
    pub versions: Vec<(String, String, String)>,
}

/// Parse arxiv.org/abs/ID page.
pub fn abs_meta(html: &str) -> AbsMeta {
    AbsMeta {
        msc: class_list(html, "msc-classes"),
        versions: versions(html),
    }
}

/// Resolve a section by id ("2.1") or title: id exact, id case-insens, title case-insens exact,
/// then title case-insens substring; the first match wins.
pub fn find_section<'a>(secs: &'a [Section], key: &str) -> Option<&'a Section> {
    let key = key.trim();
    let key = key.strip_prefix('§').unwrap_or(key).trim();
    if key.is_empty() {
        return None;
    }
    if let Some(s) = secs.iter().find(|s| s.id == key) {
        return Some(s);
    }
    if let Some(s) = secs.iter().find(|s| s.id.eq_ignore_ascii_case(key)) {
        return Some(s);
    }
    let lk = key.to_lowercase();
    if let Some(s) = secs.iter().find(|s| s.title.to_lowercase() == lk) {
        return Some(s);
    }
    secs.iter().find(|s| s.title.to_lowercase().contains(&lk))
}

/// Case-insens substring grep, or regex-lite when pat is wrapped as /re/ (supports . * + ? {n,m}
/// [] [^] ^ $ | () \d \w \s \b and their negations; `/re/i` makes it case-insensitive, the
/// default for regexes is case-sensitive; ^ and $ match at line boundaries). Hits are
/// non-overlapping, at most `max`.
pub fn grep(text: &str, secs: &[Section], pat: &str, ctx: usize, max: usize) -> Vec<Hit> {
    if max == 0 || pat.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let mut push = |pos: usize, len: usize| {
        hits.push(Hit { sec: sec_at(secs, pos).to_string(), pos, snippet: snippet(text, pos, len, ctx) });
        hits.len() < max
    };
    if let Some((re, ci)) = regex_form(pat) {
        if let Some(prog) = Regex::compile(re, ci) {
            let blown = prog.find_all(text, &mut push);
            if blown {
                hits.push(Hit { sec: String::new(), pos: 0, snippet: "(regex too complex: step budget exceeded, simplify the pattern)".into() });
            }
            return hits;
        }
    }
    literal_find_all(text, pat, &mut push);
    hits
}

// ---------------------------------------------------------------------------------------------
// small string helpers
// ---------------------------------------------------------------------------------------------

fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "copy" => '©',
        "reg" => '®',
        "deg" => '°',
        "plusmn" => '±',
        "times" => '×',
        "middot" => '·',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "larr" => '←',
        "rarr" => '→',
        "minus" => '−',
        "infin" => '∞',
        "le" => '≤',
        "ge" => '≥',
        "ne" => '≠',
        "sect" => '§',
        "para" => '¶',
        "dagger" => '†',
        "Dagger" => '‡',
        _ => return None,
    })
}

/// Decode `&amp;`-style entities. Borrows when there is nothing to decode.
fn decode_entities(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    decode_into(s, &mut out);
    Cow::Owned(out)
}

/// Append `s` with its entities decoded.
fn decode_into(s: &str, out: &mut String) {
    let mut rest = s;
    while let Some(k) = rest.find('&') {
        out.push_str(&rest[..k]);
        rest = &rest[k..];
        let semi = rest[1..].bytes().take(10).position(|b| b == b';');
        let mut done = false;
        if let Some(sp) = semi {
            let body = &rest[1..1 + sp];
            let ch = if let Some(num) = body.strip_prefix('#') {
                let v = if let Some(h) = num.strip_prefix(['x', 'X']) {
                    u32::from_str_radix(h, 16).ok()
                } else {
                    num.parse::<u32>().ok()
                };
                v.and_then(char::from_u32)
            } else {
                named_entity(body)
            };
            if let Some(c) = ch {
                out.push(c);
                rest = &rest[sp + 2..];
                done = true;
            }
        }
        if !done {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
}

/// 0 = ASCII non-whitespace, 1 = ASCII whitespace (as `char::is_whitespace`), 2 = non-ASCII.
static WS_CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = if i >= 0x80 {
            2
        } else if i == b' ' as usize || (i >= 9 && i <= 13) {
            1
        } else {
            0
        };
        i += 1;
    }
    t
};

/// First index >= `from` of byte `x` in `b`, 8 bytes per step.
fn find_byte(b: &[u8], from: usize, x: u8) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let pat = LO.wrapping_mul(u64::from(x));
    let mut i = from;
    while let Some(Ok(w)) = b.get(i..i + 8).map(<[u8; 8]>::try_from) {
        let v = u64::from_le_bytes(w) ^ pat;
        let t = v.wrapping_sub(LO) & !v & HI;
        if t != 0 {
            return Some(i + (t.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    b.get(i..)?.iter().position(|&c| c == x).map(|p| i + p)
}

/// Horspool shift table for `pat`.
const fn skip_table(pat: &[u8]) -> [u8; 256] {
    let m = pat.len();
    let mut t = [m as u8; 256];
    let mut k = 0;
    while k + 1 < m {
        t[pat[k] as usize] = (m - 1 - k) as u8;
        k += 1;
    }
    t
}

static MATH_END_LO: [u8; 256] = skip_table(b"</math");
static MATH_END_UP: [u8; 256] = skip_table(b"</MATH");
static AUTHORS: [u8; 256] = skip_table(b"ltx_authors");


/// First index >= `from` of `pat` in `b` (Horspool search with the precomputed `skip` table).
fn find_pat(b: &[u8], from: usize, pat: &[u8], skip: &[u8; 256]) -> Option<usize> {
    let m = pat.len();
    let last = pat[m - 1];
    let mut i = from;
    while i + m <= b.len() {
        let c = b[i + m - 1];
        if c == last && b[i..i + m - 1] == pat[..m - 1] {
            return Some(i);
        }
        i += usize::from(skip[c as usize]);
    }
    None
}

/// Offset of the first `<article` (the head of a page is mostly CSS / scripts with few `<`).
fn find_article(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    while let Some(k) = find_byte(b, i, b'<') {
        if b[k..].starts_with(b"<article") {
            return Some(k);
        }
        i = k + 1;
    }
    None
}

/// Index after the whitespace run starting at byte `i` (a char boundary).
fn skip_ws(t: &str, mut i: usize) -> usize {
    let b = t.as_bytes();
    while i < b.len() {
        match WS_CLASS[b[i] as usize] {
            1 => i += 1,
            0 => break,
            _ => match t[i..].chars().next() {
                Some(c) if c.is_whitespace() => i += c.len_utf8(),
                _ => break,
            },
        }
    }
    i
}

/// Index after the run of non-whitespace chars starting at byte `i` (a char boundary).
fn skip_word(t: &str, mut i: usize) -> usize {
    let b = t.as_bytes();
    while i < b.len() {
        match WS_CLASS[b[i] as usize] {
            0 => i += 1,
            1 => break,
            _ => match t[i..].chars().next() {
                Some(c) if !c.is_whitespace() => i += c.len_utf8(),
                _ => break,
            },
        }
    }
    i
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    collapse_into(&mut out, s);
    out
}

/// Append `s` with whitespace runs collapsed to one space and the ends trimmed.
pub(crate) fn collapse_into(out: &mut String, s: &str) {
    let n = s.len();
    let mut i = skip_ws(s, 0);
    let mut first = true;
    while i < n {
        let end = skip_word(s, i);
        if !first {
            out.push(' ');
        }
        first = false;
        out.push_str(&s[i..end]);
        i = skip_ws(s, end);
    }
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(k) = rest.find('<') {
        out.push_str(&rest[..k]);
        match rest[k..].find('>') {
            Some(e) => {
                // a tag boundary separates words, e.g. `<br/>` or `</td>`
                out.push(' ');
                rest = &rest[k + e + 1..];
            }
            None => return decode_entities(&out).into_owned(),
        }
    }
    out.push_str(rest);
    decode_entities(&out).into_owned()
}

/// Text of `<title>` in the head of a page.
fn head_title(html: &str) -> String {
    let Some(a) = html.find("<title") else { return String::new() };
    let Some(b) = html[a..].find('>') else { return String::new() };
    let from = a + b + 1;
    let Some(e) = html[from..].find("</title") else { return String::new() };
    let t = collapse_ws(&decode_entities(&html[from..from + e]));
    if t == "Untitled Document" {
        String::new()
    } else {
        t
    }
}

// ---------------------------------------------------------------------------------------------
// abs page
// ---------------------------------------------------------------------------------------------

fn class_list(html: &str, marker: &str) -> Vec<String> {
    let Some(k) = html.find(marker) else { return Vec::new() };
    let Some(gt) = html[k..].find('>') else { return Vec::new() };
    let from = k + gt + 1;
    let end = html[from..].find("</td").map(|e| from + e).unwrap_or_else(|| floor_boundary(html, html.len().min(from + 2000)));
    let text = strip_tags(&html[from..end]);
    text.split([',', ';']).map(|t| collapse_ws(t)).filter(|t| !t.is_empty()).collect()
}

fn month_num(m: &str) -> Option<u32> {
    const M: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let l = m.get(..3)?.to_ascii_lowercase();
    M.iter().position(|x| *x == l).map(|i| i as u32 + 1)
}

fn versions(html: &str) -> Vec<(String, String, String)> {
    let Some(k) = html.find("submission-history") else { return Vec::new() };
    let end = html[k..].find("</div>").map(|e| k + e).unwrap_or(html.len());
    let text = strip_tags(&html[k..end]);
    let mut out = Vec::new();
    // split into per-version segments at "[vN]"
    let mut marks: Vec<(usize, usize, &str)> = Vec::new();
    let mut from = 0;
    while let Some(p) = text[from..].find("[v") {
        let a = from + p;
        let digits: usize = text[a + 2..].bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 && text[a + 2 + digits..].starts_with(']') {
            marks.push((a, a + 3 + digits, &text[a + 1..a + 2 + digits]));
        }
        from = a + 2;
    }
    for (n, (_, body_start, label)) in marks.iter().enumerate() {
        let body_end = marks.get(n + 1).map(|m| m.0).unwrap_or(text.len());
        let seg = &text[*body_start..body_end];
        let toks: Vec<&str> = seg.split(|c: char| c.is_whitespace() || c == ',').filter(|t| !t.is_empty()).collect();
        let mut date = String::new();
        for w in toks.windows(3) {
            let (d, m, y) = (w[0], w[1], w[2]);
            if (1..=2).contains(&d.len())
                && d.bytes().all(|b| b.is_ascii_digit())
                && y.len() == 4
                && y.bytes().all(|b| b.is_ascii_digit())
            {
                if let Some(mn) = month_num(m) {
                    date = format!("{}-{:02}-{:02}", y, mn, d.parse::<u32>().unwrap_or(0));
                    break;
                }
            }
        }
        let mut size = String::new();
        if let Some(po) = seg.find('(') {
            let inner = &seg[po + 1..];
            let inner = &inner[..inner.find(')').unwrap_or(inner.len())];
            let num: String = inner.trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            // thousands separators ("1,102 KB") are dropped; a comma followed by a non-digit ends the number
            let mut full = num;
            let mut rest = &inner.trim_start()[full.len()..];
            while let Some(r) = rest.strip_prefix(',') {
                let more: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
                if more.is_empty() {
                    break;
                }
                full.push_str(&more);
                rest = &r[more.len()..];
            }
            let unit: String = rest.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).take(2).collect();
            if !full.is_empty() {
                size = format!("{}{}", full, unit.to_ascii_lowercase());
            }
        }
        out.push((label.to_string(), date, size));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// tags and classes
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum T {
    Div,
    Span,
    P,
    Article,
    H(u8),
    List,
    Li,
    Dt,
    Table,
    Tr,
    Cell,
    Figure,
    Caption,
    Sup,
    Sub,
    Pre,
    Block,
    SkipTag,
    Math,
    Br,
    A,
    /// anything we do not track on the stack (inline formatting, unknown, void)
    Other,
}

fn tag_of(name: &[u8]) -> T {
    let t = tag_of_lower(name);
    if t == T::Other && name.len() <= 14 && name.iter().any(u8::is_ascii_uppercase) {
        let mut buf = [0u8; 14];
        for (d, s) in buf.iter_mut().zip(name) {
            *d = s.to_ascii_lowercase();
        }
        return tag_of_lower(&buf[..name.len()]);
    }
    t
}

/// `tag_of` for a lower-case name.
fn tag_of_lower(name: &[u8]) -> T {
    match name {
        b"div" => T::Div,
        b"span" => T::Span,
        b"p" => T::P,
        b"article" => T::Article,
        b"h1" => T::H(1),
        b"h2" => T::H(2),
        b"h3" => T::H(3),
        b"h4" => T::H(4),
        b"h5" => T::H(5),
        b"h6" => T::H(6),
        b"ul" | b"ol" | b"dl" => T::List,
        b"li" => T::Li,
        b"dt" => T::Dt,
        b"table" => T::Table,
        b"tr" => T::Tr,
        b"td" | b"th" => T::Cell,
        b"figure" => T::Figure,
        b"figcaption" | b"caption" => T::Caption,
        b"sup" => T::Sup,
        b"sub" => T::Sub,
        b"pre" => T::Pre,
        b"section" | b"blockquote" | b"main" | b"center" | b"details" | b"summary" | b"address" => T::Block,
        b"script" | b"style" | b"noscript" | b"template" | b"head" | b"nav" | b"footer" | b"header" | b"dialog"
        | b"button" | b"select" | b"textarea" | b"form" | b"iframe" | b"canvas" | b"audio" | b"video"
        | b"object" | b"title" | b"desc" | b"text" | b"defs" | b"metadata" => T::SkipTag,
        b"math" => T::Math,
        b"br" => T::Br,
        b"a" => T::A,
        _ => T::Other,
    }
}

const B_WS: u8 = 1;
const B_NAME: u8 = 2;
const B_ATTR_END: u8 = 4;

/// Byte classes for the tag scanner: ASCII whitespace, tag-name characters, attribute-name terminators.
static BYTE_CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let c = i as u8;
        if c.is_ascii_whitespace() {
            t[i] |= B_WS;
        }
        if c.is_ascii_alphanumeric() || c == b'-' || c == b':' || c == b'_' {
            t[i] |= B_NAME;
        }
        if matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'=' | b'>' | b'/') {
            t[i] |= B_ATTR_END;
        }
        i += 1;
    }
    t
};

/// Open elements tracked at most; deeper tags are ignored. Closers only search the top MAX_SCAN frames.
const MAX_DEPTH: usize = 4096;
const MAX_SCAN: usize = 64;

const F_SKIP: u64 = 1 << 0;
const F_THEOREM: u64 = 1 << 1;
const F_TITLE: u64 = 1 << 2;
const F_RUNIN: u64 = 1 << 3;
const F_BIBITEM: u64 = 1 << 4;
const F_BIBBLOCK: u64 = 1 << 5;
const F_BIBLIST: u64 = 1 << 6;
const F_TAG: u64 = 1 << 7;
const F_TAG_BIBITEM: u64 = 1 << 8;
const F_TAG_ITEM: u64 = 1 << 9;
const F_AUTHORS: u64 = 1 << 10;
const F_PERSON: u64 = 1 << 11;
const F_R_AFFIL: u64 = 1 << 12;
const F_R_ADDR: u64 = 1 << 13;
const F_R_THANKS: u64 = 1 << 14;
const F_R_FN: u64 = 1 << 15;
const F_R_FNMARK: u64 = 1 << 16;
const F_NOTE: u64 = 1 << 17;
const F_EQ: u64 = 1 << 18;
const F_LISTING: u64 = 1 << 19;
const F_LISTINGLINE: u64 = 1 << 20;
const F_VERBATIM: u64 = 1 << 21;
const F_INLINE: u64 = 1 << 22;
const F_FIG_TABLE: u64 = 1 << 23;
const F_FIG_LISTING: u64 = 1 << 24;
const F_FIG_ALGO: u64 = 1 << 25;

#[derive(Default, Clone, Copy)]
struct Cls<'a> {
    flags: u64,
    /// suffix of `ltx_theorem_<kind>`
    thm_kind: &'a str,
    /// suffix of `ltx_title_<kind>`
    title_kind: &'a str,
}

impl Cls<'_> {
    fn has(&self, f: u64) -> bool {
        self.flags & f != 0
    }
}

fn classify(class: &str) -> Cls<'_> {
    let mut c = Cls::default();
    for tok in class.split_ascii_whitespace() {
        let Some(r) = tok.strip_prefix("ltx_") else { continue };
        // first two bytes of every recognised class: the rest (ltx_text, ltx_para, ltx_font_*, ...) is skipped early
        let rb = r.as_bytes();
        if rb.len() < 2
            || !matches!(
                (rb[0], rb[1]),
                (b't', b'h' | b'i' | b'a' | b'o')
                    | (b'r', b'u' | b'o')
                    | (b'b', b'i')
                    | (b'a', b'u' | b'l')
                    | (b'p', b'e' | b'a')
                    | (b'n', b'o')
                    | (b'e', b'q')
                    | (b'l', b'i' | b's' | b'o')
                    | (b'v', b'e')
                    | (b'i', b'n')
                    | (b'f', b'l')
                    | (b'd', b'a')
                    | (b'T', b'O')
                    | (b'c', b'o')
            )
        {
            continue;
        }
        c.flags |= match r {
            "theorem" => F_THEOREM,
            "title" => F_TITLE,
            "runin" => F_RUNIN,
            "bibitem" => F_BIBITEM,
            "bibblock" => F_BIBBLOCK,
            "biblist" => F_BIBLIST,
            "tag" => F_TAG,
            "tag_bibitem" => F_TAG_BIBITEM | F_TAG,
            "tag_item" => F_TAG_ITEM | F_TAG,
            "authors" => F_AUTHORS,
            "personname" => F_PERSON,
            "role_affiliation" | "role_institutetext" | "role_affiliationtext" => F_R_AFFIL,
            "role_address" => F_R_ADDR,
            "role_thanks" => F_R_THANKS,
            "role_footnote" | "role_footnotetext" => F_R_FN,
            "role_footnotemark" => F_R_FNMARK,
            "note" => F_NOTE,
            "equation" | "equationgroup" | "eqn_table" => F_EQ,
            "listing" | "lstlisting" => F_LISTING | F_FIG_LISTING,
            "listingline" => F_LISTINGLINE,
            "verbatim" => F_VERBATIM,
            "inline-block" | "inline-para" | "logical-block" => F_INLINE,
            "table" => F_FIG_TABLE,
            "float_algorithm" | "algorithm" => F_FIG_ALGO,
            "page_navbar" | "page_footer" | "listing_data" | "dates" | "toclist" | "tocentry" | "TOC"
            | "contact_name" | "note_mark" | "note_type" | "tag_note" | "tag_equation" | "eqn_eqno" => F_SKIP,
            _ => {
                if let Some(k) = r.strip_prefix("theorem_") {
                    c.thm_kind = k;
                } else if let Some(k) = r.strip_prefix("title_") {
                    c.title_kind = k;
                }
                0
            }
        };
    }
    c
}

struct Attrs<'a> {
    class: &'a str,
    alt: &'a str,
    display: &'a str,
    href: &'a str,
    self_close: bool,
    /// `<script>` / `<style>`: closing-tag needles whose content is skipped wholesale
    raw: Option<(&'static str, &'static str)>,
}

#[derive(Clone, Copy)]
enum Act {
    None,
    Skip,
    Block,
    Article,
    Authors,
    /// 0 = person name, 1 = affiliation, 2 = address, 3 = thanks
    Contact(u8),
    Heading,
    HTag,
    Runin { start: usize, thm: bool },
    Theorem(usize),
    Note,
    Fig,
    Caption { start: usize },
    Table,
    EqTable,
    Row,
    Cell,
    List,
    Li,
    Dt,
    ItemTag(usize),
    BibItem,
    BibTag,
    BibBlock,
    Sup(usize),
    Sub(usize),
    Listing,
    ListingLine,
    Pre,
}

#[derive(Clone, Copy)]
struct Frame {
    tag: T,
    act: Act,
}

#[derive(Clone, Copy, PartialEq)]
enum HeadKind {
    Doc,
    Section(u8),
    Bold,
}

struct HeadState {
    kind: HeadKind,
    line_start: usize,
    content_start: usize,
    tag_start: usize,
    tag_end: Option<usize>,
}

struct BibState {
    start: usize,
    text_start: usize,
    tag: String,
    hrefs: String,
}

struct ItemBuild<'a> {
    start: usize,
    end: usize,
    body_start: usize,
    label: String,
    class_kind: &'a str,
    /// index into `secs` of the enclosing section
    sec: Option<usize>,
}

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
    md: String,
    sp: bool,
    need_break: u8,
    runin: bool,
    skip: u32,
    capskip: u32,
    cap_depth: u32,
    cap: String,
    cap_sp: bool,
    stack: Vec<Frame>,
    head: Option<HeadState>,
    title: String,
    secs: Vec<Section>,
    sec_open: Vec<usize>,
    sec_seq: u32,
    items: Vec<ItemBuild<'a>>,
    thm_stack: Vec<usize>,
    /// Largest end offset of a closed theorem: later cleanup can cut markdown back below it, and text
    /// written there is then read as part of the item.
    item_hi: usize,
    pending_kind: &'a str,
    refs: Vec<Ref>,
    bib: Option<BibState>,
    cell_depth: u32,
    li_depth: u32,
    list_depth: u32,
    levels: Option<(u8, u8)>,
    want_md: bool,
    want_items: bool,
    want_refs: bool,
    /// open `<sup>` / `<sub>` frames (closing one inserts a byte, shifting every offset recorded inside)
    supsub: u32,
    /// reusable buffers for decoded text and cleaned TeX
    dec: String,
    tex: String,
    /// buffers of the last finished bibliography entry, reused by the next one
    spare_tag: String,
    spare_hrefs: String,
    eq_depth: u32,
    keep_ws: u8,
    first_line: bool,
    row_stack: Vec<(usize, u32)>,
    fig_kind: Vec<&'static str>,
    insts: Vec<String>,
    /// author names, one per line
    names: String,
    /// last `classify` result per class-attribute hash slot (pages repeat a few dozen class strings)
    cls_cache: Vec<(&'a str, Cls<'a>)>,
    authors: u32,
    stop_after_authors: bool,
    done: bool,
}

fn decode_attr(v: &str) -> Cow<'_, str> {
    decode_entities(v)
}

impl<'a> Parser<'a> {
    fn new(s: &'a str, want: Want, md_cap: usize) -> Self {
        let want_md = matches!(want, Want::All | Want::Text);
        Parser {
            s,
            b: s.as_bytes(),
            i: 0,
            md: String::with_capacity(md_cap),
            sp: false,
            need_break: 0,
            runin: false,
            skip: 0,
            capskip: 0,
            cap_depth: 0,
            cap: String::new(),
            cap_sp: false,
            stack: Vec::with_capacity(64),
            head: None,
            title: String::new(),
            secs: Vec::new(),
            sec_open: Vec::new(),
            sec_seq: 0,
            items: Vec::new(),
            thm_stack: Vec::new(),
            item_hi: 0,
            pending_kind: "",
            refs: Vec::new(),
            bib: None,
            cell_depth: 0,
            li_depth: 0,
            list_depth: 0,
            levels: None,
            want_md,
            want_items: matches!(want, Want::All | Want::Items),
            want_refs: matches!(want, Want::All | Want::Refs),
            supsub: 0,
            dec: String::new(),
            tex: String::new(),
            spare_tag: String::new(),
            spare_hrefs: String::new(),
            eq_depth: 0,
            keep_ws: 0,
            first_line: false,
            row_stack: Vec::new(),
            fig_kind: Vec::new(),
            insts: Vec::new(),
            names: String::new(),
            cls_cache: Vec::new(),
            authors: 0,
            stop_after_authors: false,
            done: false,
        }
    }

    // ------------------------------------------------------------------ tokenizer

    fn run(&mut self) {
        let n = self.b.len();
        while self.i < n && !self.done {
            let s = self.s;
            match find_byte(self.b, self.i, b'<') {
                None => {
                    self.text(&s[self.i..]);
                    self.i = n;
                }
                Some(k) if k == self.i => self.tag(),
                Some(k) => {
                    self.text(&s[self.i..k]);
                    self.i = k;
                    self.tag();
                }
            }
        }
        while let Some(f) = self.stack.pop() {
            self.close_frame(f);
        }
    }

    /// Handle the construct starting at `self.i` (a '<').
    fn tag(&mut self) {
        let s = self.s;
        let b = self.b;
        let n = b.len();
        let st = self.i;
        if st + 1 >= n {
            self.text("<");
            self.i = n;
            return;
        }
        let c = b[st + 1];
        if c == b'!' {
            if s[st..].starts_with("<!--") {
                self.i = s[st + 4..].find("-->").map(|e| st + 4 + e + 3).unwrap_or(n);
            } else {
                self.i = s[st..].find('>').map(|e| st + e + 1).unwrap_or(n);
            }
            return;
        }
        if c == b'?' {
            self.i = s[st..].find('>').map(|e| st + e + 1).unwrap_or(n);
            return;
        }
        let closing = c == b'/';
        let ns = st + 1 + closing as usize;
        if ns >= n || !b[ns].is_ascii_alphabetic() {
            self.text("<");
            self.i = st + 1;
            return;
        }
        let mut j = ns;
        while j < n && BYTE_CLASS[b[j] as usize] & B_NAME != 0 {
            j += 1;
        }
        let t = tag_of(&b[ns..j]);
        if closing {
            self.i = if b.get(j) == Some(&b'>') { j + 1 } else { s[j..].find('>').map(|e| j + e + 1).unwrap_or(n) };
            self.close_tag(t);
            return;
        }
        let mut a = Attrs { class: "", alt: "", display: "", href: "", self_close: false, raw: None };
        if t == T::SkipTag {
            let nm = &b[ns..j];
            if nm.eq_ignore_ascii_case(b"script") {
                a.raw = Some(("</script", "</SCRIPT"));
            } else if nm.eq_ignore_ascii_case(b"style") {
                a.raw = Some(("</style", "</STYLE"));
            }
        }
        loop {
            while j < n && BYTE_CLASS[b[j] as usize] & B_WS != 0 {
                j += 1;
            }
            if j >= n {
                self.i = n;
                return;
            }
            match b[j] {
                b'>' => {
                    j += 1;
                    break;
                }
                b'/' => {
                    a.self_close = true;
                    j += 1;
                    continue;
                }
                _ => {}
            }
            let an = j;
            while j < n && BYTE_CLASS[b[j] as usize] & B_ATTR_END == 0 {
                j += 1;
            }
            if j == an {
                j += 1;
                continue;
            }
            let aname = &b[an..j];
            while j < n && BYTE_CLASS[b[j] as usize] & B_WS != 0 {
                j += 1;
            }
            let mut val = "";
            if j < n && b[j] == b'=' {
                j += 1;
                while j < n && BYTE_CLASS[b[j] as usize] & B_WS != 0 {
                    j += 1;
                }
                if j < n && (b[j] == b'"' || b[j] == b'\'') {
                    match find_byte(b, j + 1, b[j]) {
                        Some(e) => {
                            val = &s[j + 1..e];
                            j = e + 1;
                        }
                        None => {
                            self.i = n;
                            return;
                        }
                    }
                } else {
                    let vs = j;
                    while j < n && BYTE_CLASS[b[j] as usize] & B_WS == 0 && b[j] != b'>' {
                        j += 1;
                    }
                    val = &s[vs..j];
                }
            }
            match aname.len() {
                5 if aname.eq_ignore_ascii_case(b"class") => a.class = val,
                7 if aname.eq_ignore_ascii_case(b"alttext") => a.alt = val,
                7 if aname.eq_ignore_ascii_case(b"display") => a.display = val,
                4 if aname.eq_ignore_ascii_case(b"href") => a.href = val,
                _ => {}
            }
        }
        self.i = j;
        self.open_tag(t, &a);
    }

    fn skip_to_close(&mut self, lower: &str, upper: &str) {
        let from = self.i;
        self.i = match self.s[from..].find(lower).or_else(|| self.s[from..].find(upper)) {
            Some(e) => {
                let at = from + e;
                self.s[at..].find('>').map(|g| at + g + 1).unwrap_or(self.b.len())
            }
            None => self.b.len(),
        };
    }

    // ------------------------------------------------------------------ tree events

    fn open_tag(&mut self, t: T, a: &Attrs<'a>) {
        match t {
            T::Br => {
                if self.cap_depth > 0 {
                    if self.capskip == 0 {
                        self.cap.push('\n');
                        self.cap_sp = false;
                    }
                } else if self.skip == 0 {
                    if self.cell_depth > 0 || self.head.is_some() {
                        self.sp = true;
                    } else if self.keep_ws > 0 {
                        self.md.push('\n');
                    } else {
                        self.line_break();
                    }
                }
                return;
            }
            T::Math => {
                if self.skip == 0 {
                    self.math(a.alt, a.display, self.i);
                }
                self.skip_math();
                return;
            }
            T::A => {
                if let Some(bib) = self.bib.as_mut() {
                    if self.want_refs && !a.href.is_empty() && self.skip == 0 {
                        bib.hrefs.push(' ');
                        bib.hrefs.push_str(&decode_attr(a.href));
                    }
                }
                return;
            }
            T::Other => return,
            _ => {}
        }
        if self.stack.len() >= MAX_DEPTH {
            return;
        }
        match t {
            T::Li => self.implied_close(T::Li, &[T::List]),
            T::P => {
                if matches!(self.stack.last(), Some(Frame { tag: T::P, .. })) {
                    let f = self.stack.pop().unwrap();
                    self.close_frame(f);
                }
            }
            T::Cell => self.implied_close(T::Cell, &[T::Table, T::Tr]),
            T::Tr => self.implied_close(T::Tr, &[T::Table]),
            _ => {}
        }
        let act = self.decide(t, a);
        let act = self.enter(act);
        if a.self_close {
            self.close_frame(Frame { tag: t, act });
            return;
        }
        self.stack.push(Frame { tag: t, act });
        if let Some((lo, up)) = a.raw {
            self.skip_to_close(lo, up);
            let f = self.stack.pop().unwrap();
            self.close_frame(f);
        }
    }

    fn close_tag(&mut self, t: T) {
        if matches!(t, T::Other | T::Math | T::Br | T::A) {
            return;
        }
        // junk input can leave thousands of unclosed frames: only look near the top
        let lo = self.stack.len().saturating_sub(MAX_SCAN);
        let Some(pos) = self.stack[lo..].iter().rposition(|f| f.tag == t).map(|p| p + lo) else { return };
        while self.stack.len() > pos {
            let f = self.stack.pop().unwrap();
            self.close_frame(f);
        }
    }

    /// Close the nearest open `t` (above any `barrier`) for implied end tags (`<li>` after `<li>`).
    fn implied_close(&mut self, t: T, barrier: &[T]) {
        for k in (self.stack.len().saturating_sub(MAX_SCAN)..self.stack.len()).rev() {
            let ft = self.stack[k].tag;
            if ft == t {
                while self.stack.len() > k {
                    let f = self.stack.pop().unwrap();
                    self.close_frame(f);
                }
                return;
            }
            if barrier.contains(&ft) {
                return;
            }
        }
    }

    fn classify_cached(&mut self, class: &'a str) -> Cls<'a> {
        if self.stop_after_authors {
            return classify(class);
        }
        if self.cls_cache.is_empty() {
            self.cls_cache = vec![("", Cls::default()); 128];
        }
        let b = class.as_bytes();
        let n = b.len();
        let slot = (n * 31 + usize::from(b[0]) * 7 + usize::from(b[n / 2]) * 3 + usize::from(b[n - 1])) & 127;
        let e = &mut self.cls_cache[slot];
        if e.0 != class {
            *e = (class, classify(class));
        }
        e.1
    }

    fn decide(&mut self, t: T, a: &Attrs<'a>) -> Act {
        let want_class = !a.class.is_empty() && (self.skip == 0 || self.authors > 0);
        let c = if want_class { self.classify_cached(a.class) } else { Cls::default() };
        if t == T::SkipTag || c.has(F_SKIP) {
            return Act::Skip;
        }
        if self.skip > 0 {
            if self.authors > 0 {
                if c.has(F_R_THANKS) {
                    return Act::Contact(3);
                }
                if c.has(F_R_AFFIL) {
                    return Act::Contact(1);
                }
                if c.has(F_R_ADDR) {
                    return Act::Contact(2);
                }
                if c.has(F_PERSON) {
                    return Act::Contact(0);
                }
                if c.has(F_NOTE) || t == T::Sup {
                    return Act::Skip;
                }
            }
            return Act::None;
        }
        if c.has(F_AUTHORS) {
            return Act::Authors;
        }
        if c.has(F_R_FNMARK) {
            return Act::Skip;
        }
        if c.has(F_R_FN) || (c.has(F_NOTE) && t == T::Span) {
            return if self.head.is_some() { Act::Skip } else { Act::Note };
        }
        if let T::H(n) = t {
            return self.decide_heading(n, &c);
        }
        if c.has(F_THEOREM) && t == T::Div {
            self.pending_kind = c.thm_kind;
            return Act::Theorem(0);
        }
        if c.has(F_BIBITEM) {
            return Act::BibItem;
        }
        if c.has(F_BIBBLOCK) {
            return Act::BibBlock;
        }
        if c.has(F_TAG) {
            if self.head.is_some() {
                return Act::HTag;
            }
            if self.bib.is_some() && c.has(F_TAG_BIBITEM) {
                return Act::BibTag;
            }
            if self.li_depth > 0 && c.has(F_TAG_ITEM) {
                return Act::ItemTag(self.md.len());
            }
            return Act::None;
        }
        if c.has(F_LISTINGLINE) {
            return Act::ListingLine;
        }
        if c.has(F_LISTING) && t == T::Div {
            return Act::Listing;
        }
        if c.has(F_VERBATIM) {
            return Act::Pre;
        }
        match t {
            T::Table => {
                if c.has(F_EQ) {
                    Act::EqTable
                } else {
                    Act::Table
                }
            }
            T::Tr => {
                if self.eq_depth > 0 {
                    Act::None
                } else {
                    Act::Row
                }
            }
            T::Cell => {
                if self.eq_depth > 0 {
                    Act::None
                } else {
                    Act::Cell
                }
            }
            T::Figure => {
                let kind = if c.has(F_FIG_TABLE) {
                    "table"
                } else if c.has(F_FIG_ALGO) {
                    "algorithm"
                } else if c.has(F_FIG_LISTING) {
                    "listing"
                } else {
                    "fig"
                };
                self.fig_kind.push(kind);
                Act::Fig
            }
            T::Caption => Act::Caption { start: 0 },
            T::List => {
                if c.has(F_BIBLIST) {
                    Act::Block
                } else {
                    Act::List
                }
            }
            T::Li => Act::Li,
            T::Dt => Act::Dt,
            T::Sup => Act::Sup(0),
            T::Sub => Act::Sub(0),
            T::Pre => Act::Pre,
            T::Article => Act::Article,
            T::Div if c.has(F_INLINE) => Act::None,
            T::Div | T::P | T::Block => Act::Block,
            _ => Act::None,
        }
    }

    /// Heading levels: part/chapter exist only in some documents; `section` sits below whichever are present.
    /// Returns (chapter level, section level).
    fn levels(&mut self) -> (u8, u8) {
        let b = self.b;
        *self.levels.get_or_insert_with(|| {
            // every `ltx_title_*` has an `x`, the one rare byte to anchor on
            let (mut part, mut chapter) = (0, 0);
            let mut i = 0;
            while let Some(k) = find_byte(b, i, b'x') {
                i = k + 1;
                if k >= 2 && b[k - 2..].starts_with(b"ltx_title_") {
                    let rest = &b[k + 8..];
                    part |= u8::from(rest.starts_with(b"part"));
                    chapter |= u8::from(rest.starts_with(b"chapter"));
                    if part & chapter == 1 {
                        break;
                    }
                }
            }
            (1 + part, 1 + part + chapter)
        })
    }

    fn decide_heading(&mut self, n: u8, c: &Cls<'a>) -> Act {
        let kind = match c.title_kind {
            "document" => {
                if self.title.is_empty() {
                    HeadKind::Doc
                } else {
                    HeadKind::Bold
                }
            }
            "part" => HeadKind::Section(1),
            "chapter" => HeadKind::Section(self.levels().0),
            "section" => HeadKind::Section(self.levels().1),
            // back matter (and book appendices) sit at chapter level when the document has chapters
            "appendix" | "bibliography" | "abstract" | "acknowledgements" | "index" | "glossary" => {
                let (chapter, section) = self.levels();
                HeadKind::Section(chapter.min(section))
            }
            "subsection" | "subappendix" => HeadKind::Section(self.levels().1 + 1),
            "subsubsection" => HeadKind::Section(self.levels().1 + 2),
            "paragraph" | "subparagraph" | "theorem" | "proof" => return self.heading_runin(c),
            "" if !c.has(F_TITLE) => {
                if n == 1 && self.title.is_empty() {
                    HeadKind::Doc
                } else {
                    HeadKind::Section(n.saturating_sub(1).clamp(1, 3))
                }
            }
            _ => {
                if c.has(F_RUNIN) {
                    return self.heading_runin(c);
                }
                HeadKind::Bold
            }
        };
        if let HeadKind::Section(level) = kind {
            self.close_sections(level);
        }
        self.block_break();
        self.flush_break();
        let line_start = self.md.len();
        match kind {
            HeadKind::Doc => self.md.push_str("# "),
            HeadKind::Section(l) => {
                for _ in 0..=l {
                    self.md.push('#');
                }
                self.md.push(' ');
            }
            HeadKind::Bold => self.md.push_str("**"),
        }
        self.sp = false;
        self.runin = false;
        let cs = self.md.len();
        self.head = Some(HeadState { kind, line_start, content_start: cs, tag_start: cs, tag_end: None });
        Act::Heading
    }

    fn heading_runin(&mut self, c: &Cls) -> Act {
        self.block_break();
        self.flush_break();
        self.md.push_str("**");
        self.sp = false;
        self.runin = false;
        Act::Runin { start: self.md.len(), thm: c.title_kind == "theorem" }
    }

    /// Side effects of entering a frame (pairs with `close_frame`).
    fn enter(&mut self, act: Act) -> Act {
        let mut act = act;
        match act {
            Act::Skip => {
                self.skip += 1;
                self.capskip += 1;
            }
            Act::Authors => {
                self.authors += 1;
                self.skip += 1;
            }
            Act::Contact(_) => {
                if self.cap_depth == 0 {
                    self.cap.clear();
                    self.cap_sp = false;
                }
                self.cap_depth += 1;
            }
            Act::Block | Act::Article => self.block_break(),
            Act::Theorem(_) => {
                self.block_break();
                self.flush_break();
                if self.want_items {
                    let sec = self.sec_open.last().copied();
                    let at = self.md.len();
                    self.thm_stack.push(self.items.len());
                    act = Act::Theorem(self.items.len());
                    self.items.push(ItemBuild {
                        start: at,
                        end: at,
                        body_start: at,
                        label: String::new(),
                        class_kind: self.pending_kind,
                        sec,
                    });
                }
            }
            Act::Note => {
                self.sp = true;
                self.flush_text();
                self.md.push_str("[fn: ");
                self.sp = false;
            }
            Act::Fig | Act::Table | Act::EqTable | Act::List => {
                self.runin = false;
                self.block_break();
                match act {
                    Act::EqTable => self.eq_depth += 1,
                    Act::List => self.list_depth += 1,
                    _ => {}
                }
            }
            Act::Caption { .. } => {
                self.block_break();
                self.flush_break();
                let kind = self.fig_kind.last().copied().unwrap_or("table");
                self.md.push('[');
                self.md.push_str(kind);
                self.md.push_str(": ");
                self.sp = false;
                act = Act::Caption { start: self.md.len() };
            }
            Act::Row => {
                self.runin = false;
                self.line_break();
                self.flush_break();
                self.row_stack.push((self.md.len(), 0));
            }
            Act::Cell => {
                self.cell_depth += 1;
                self.flush_break();
                let at = self.md.len();
                if let Some((start, cells)) = self.row_stack.last_mut() {
                    if *cells > 0 {
                        self.md.push_str(if at == *start { "| " } else { " | " });
                    }
                    *cells += 1;
                }
                self.sp = false;
            }
            Act::Li => {
                self.li_depth += 1;
                if self.cell_depth > 0 {
                    self.sp = true;
                } else {
                    self.runin = false;
                    self.line_break();
                    self.flush_break();
                    for _ in 1..self.list_depth {
                        self.md.push_str("  ");
                    }
                    self.md.push_str("- ");
                    self.sp = false;
                    self.runin = true;
                }
            }
            Act::Dt => {
                self.runin = false;
                self.line_break();
                self.flush_break();
                self.md.push_str("- ");
                self.sp = false;
                self.runin = true;
            }
            Act::HTag => {
                if let Some(h) = self.head.as_mut() {
                    h.tag_start = self.md.len();
                }
            }
            Act::BibItem => {
                self.need_break = self.need_break.max(1);
                self.runin = false;
                self.flush_break();
                let at = self.md.len();
                let (tag, hrefs) = (std::mem::take(&mut self.spare_tag), std::mem::take(&mut self.spare_hrefs));
                self.bib = Some(BibState { start: at, text_start: at, tag, hrefs });
            }
            Act::BibBlock => self.sp = true,
            Act::Sup(_) | Act::Sub(_) => {
                self.sp = false;
                self.flush_break();
                let sup = matches!(act, Act::Sup(_));
                self.supsub += 1;
                self.md.push(if sup { '^' } else { '_' });
                act = if sup { Act::Sup(self.md.len()) } else { Act::Sub(self.md.len()) };
            }
            Act::Listing => {
                self.runin = false;
                self.block_break();
                self.keep_ws = 1;
                self.first_line = true;
            }
            Act::Pre => {
                self.runin = false;
                self.block_break();
                self.flush_break();
                self.keep_ws = 2;
            }
            Act::ListingLine => {
                self.runin = false;
                if self.first_line {
                    self.first_line = false;
                    self.flush_break();
                } else {
                    self.need_break = 0;
                    self.md.push('\n');
                }
            }
            Act::BibTag | Act::ItemTag(_) | Act::None | Act::Heading | Act::Runin { .. } => {}
        }
        act
    }

    fn close_frame(&mut self, f: Frame) {
        match f.act {
            Act::None => {}
            Act::Skip => {
                self.skip -= 1;
                self.capskip -= 1;
            }
            Act::Authors => {
                self.authors -= 1;
                self.skip -= 1;
                if self.stop_after_authors && self.authors == 0 {
                    self.done = true;
                }
            }
            Act::Contact(kind) => {
                self.cap_depth -= 1;
                if self.cap_depth == 0 {
                    let raw = std::mem::take(&mut self.cap);
                    self.finish_contact(kind, &raw);
                    self.cap = raw;
                }
            }
            Act::Block => self.block_break(),
            Act::Article => {
                self.block_break();
                self.done = true;
            }
            Act::Heading => self.close_heading(),
            Act::HTag => {
                if let Some(h) = self.head.as_mut() {
                    h.tag_end = Some(self.md.len());
                }
                self.sp = true;
            }
            Act::Runin { start, thm } => self.close_runin(start, thm),
            Act::Theorem(idx) => {
                self.block_break();
                if self.want_items {
                    self.thm_stack.pop();
                    self.items[idx].end = self.md.len();
                    self.item_hi = self.item_hi.max(self.md.len());
                }
            }
            Act::Note => {
                while self.md.ends_with(' ') {
                    self.md.pop();
                }
                self.md.push(']');
                self.sp = true;
            }
            Act::Fig => {
                self.fig_kind.pop();
                self.block_break();
            }
            Act::Caption { start } => {
                while self.md.len() > start && self.md.ends_with(' ') {
                    self.md.pop();
                }
                if self.md.len() <= start {
                    let at = self.md[..start.min(self.md.len())].rfind('[').unwrap_or(0);
                    self.md.truncate(at);
                } else {
                    self.md.push(']');
                }
                self.block_break();
            }
            Act::Table => self.block_break(),
            Act::EqTable => {
                self.eq_depth -= 1;
                self.block_break();
            }
            Act::Row => {
                if let Some((start, _)) = self.row_stack.pop() {
                    while self.md.len() > start && self.md.ends_with([' ', '|']) {
                        self.md.pop();
                    }
                    if self.md.len() <= start || self.md[start..].trim().is_empty() {
                        self.md.truncate(start);
                    }
                }
                self.line_break();
            }
            Act::Cell => {
                self.cell_depth = self.cell_depth.saturating_sub(1);
                self.sp = true;
            }
            Act::List => {
                self.list_depth = self.list_depth.saturating_sub(1);
                self.block_break();
            }
            Act::Li => {
                self.li_depth = self.li_depth.saturating_sub(1);
                self.line_break();
            }
            Act::Dt => {
                self.runin = false;
                while self.md.ends_with(' ') {
                    self.md.pop();
                }
                if !self.md.ends_with("- ") {
                    self.md.push(':');
                }
                self.sp = true;
            }
            Act::ItemTag(start) => {
                if start <= self.md.len() {
                    let t = self.md[start..].trim();
                    let mut ch = t.chars();
                    if let (Some(c0), None) = (ch.next(), ch.next()) {
                        if !c0.is_alphanumeric() {
                            self.md.truncate(start);
                        }
                    }
                }
                self.sp = true;
                self.runin = true;
            }
            Act::BibItem => {
                if let Some(mut b) = self.bib.take() {
                    if self.want_refs {
                        let text = collapse_ws(&self.md[b.text_start.min(self.md.len())..]);
                        let n = self.refs.len() as u32 + 1;
                        self.refs.push(make_ref_with(n, &b.tag, text, &b.hrefs, &mut self.dec));
                    }
                    b.tag.clear();
                    b.hrefs.clear();
                    self.spare_tag = b.tag;
                    self.spare_hrefs = b.hrefs;
                }
                self.line_break();
            }
            Act::BibTag => {
                if self.want_refs {
                    if let Some(b) = self.bib.as_mut() {
                        b.tag.clear();
                        b.tag.push_str(self.md[b.start.min(self.md.len())..].trim());
                        b.text_start = self.md.len();
                    }
                }
                self.sp = true;
            }
            Act::BibBlock => self.sp = true,
            Act::Sup(start) | Act::Sub(start) => {
                self.supsub -= 1;
                if start <= self.md.len() {
                    let n = self.md.len() - start;
                    if n == 0 {
                        self.md.pop();
                    } else if self.md[start..].chars().nth(1).is_some() {
                        self.md.insert(start, '(');
                        self.md.push(')');
                    }
                }
            }
            Act::Listing | Act::Pre => {
                self.keep_ws = 0;
                self.block_break();
            }
            Act::ListingLine => {}
        }
    }

    // ------------------------------------------------------------------ headings, runin titles

    fn close_heading(&mut self) {
        let Some(h) = self.head.take() else { return };
        let end_tag = h.tag_end;
        let empty = self.md[h.content_start.min(self.md.len())..].trim().is_empty();
        while self.md.ends_with(' ') {
            self.md.pop();
        }
        match h.kind {
            HeadKind::Bold => {
                if empty {
                    self.md.truncate(h.line_start);
                } else {
                    self.md.push_str("**");
                }
                self.block_break();
            }
            HeadKind::Doc => {
                if empty {
                    self.md.truncate(h.line_start);
                } else {
                    self.title = self.md[h.content_start..].trim().to_string();
                }
                self.block_break();
            }
            HeadKind::Section(level) => {
                if empty {
                    self.md.truncate(h.line_start);
                    self.block_break();
                    return;
                }
                let (id, title) = match end_tag {
                    Some(te) if te > h.tag_start && te <= self.md.len() => {
                        (section_id_from_tag(self.md[h.tag_start..te].trim()), self.md[te..].trim().to_string())
                    }
                    _ => (None, self.md[h.content_start..].trim().to_string()),
                };
                let id = id.unwrap_or_else(|| {
                    self.sec_seq += 1;
                    format!("s{}", self.sec_seq)
                });
                self.secs.push(Section { id, level, title, start: h.line_start, end: h.line_start });
                self.sec_open.push(self.secs.len() - 1);
                self.block_break();
            }
        }
    }

    /// Close every open section whose level is >= `level` (a heading of that level starts now).
    fn close_sections(&mut self, level: u8) {
        while let Some(&k) = self.sec_open.last() {
            if self.secs[k].level < level {
                break;
            }
            self.sec_open.pop();
            let end = self.md.trim_end().len().max(self.secs[k].start);
            self.secs[k].end = end;
        }
    }

    fn close_runin(&mut self, start: usize, thm: bool) {
        while self.md.len() > start && self.md.ends_with(' ') {
            self.md.pop();
        }
        if self.md.len() <= start {
            self.md.truncate(start.saturating_sub(2));
            return;
        }
        if thm && self.want_items {
            let label = self.md[start..].trim_end_matches(['.', ':', ' ']).to_string();
            if let Some(&k) = self.thm_stack.last() {
                self.items[k].label = label;
            }
        }
        self.md.push_str("**");
        self.sp = true;
        self.runin = true;
        if thm {
            let at = self.md.len();
            if let Some(&k) = self.thm_stack.last() {
                self.items[k].body_start = at;
            }
        }
    }

    // ------------------------------------------------------------------ output primitives

    fn block_break(&mut self) {
        if self.runin {
            return;
        }
        if self.cell_depth > 0 {
            self.sp = true;
        } else if self.li_depth > 0 {
            self.need_break = self.need_break.max(1);
        } else {
            self.need_break = 2;
        }
    }

    fn line_break(&mut self) {
        if self.runin {
            return;
        }
        if self.cell_depth > 0 {
            self.sp = true;
        } else {
            self.need_break = self.need_break.max(1);
        }
    }

    fn flush_break(&mut self) {
        let nb = self.need_break as usize;
        if nb == 0 {
            return;
        }
        self.need_break = 0;
        self.sp = false;
        if self.md.is_empty() {
            return;
        }
        let have = self.md.bytes().rev().take(nb).take_while(|&b| b == b'\n').count();
        for _ in have..nb {
            self.md.push('\n');
        }
    }

    /// Prepare to append visible text: pending breaks, pending single space.
    fn flush_text(&mut self) {
        if self.need_break > 0 {
            self.flush_break();
        } else if self.sp {
            if let Some(&last) = self.md.as_bytes().last() {
                if last != b' ' && last != b'\n' {
                    self.md.push(' ');
                }
            }
        }
        self.sp = false;
        self.runin = false;
    }

    fn text(&mut self, raw: &str) {
        if self.cap_depth > 0 {
            if self.capskip == 0 {
                self.cap_text(&decode_entities(raw));
            }
            return;
        }
        if self.skip > 0 {
            return;
        }
        if self.keep_ws == 0 && find_byte(raw.as_bytes(), 0, b'&').is_none() {
            if self.emit() {
                self.words(raw);
            } else {
                self.text_unwritten(raw);
            }
            return;
        }
        let mut dec = std::mem::take(&mut self.dec);
        let t = if find_byte(raw.as_bytes(), 0, b'&').is_some() {
            dec.clear();
            decode_into(raw, &mut dec);
            dec.as_str()
        } else {
            raw
        };
        if self.keep_ws > 0 {
            self.raw_text(t);
        } else if self.emit() {
            self.words(t);
        } else {
            self.text_unwritten(t);
        }
        self.dec = dec;
    }

    /// Markdown nobody reads (outside headings / theorems / bibliography entries when only those are
    /// wanted) still has to move the layout state exactly as the real text would, and leave the same
    /// kind of last byte; one placeholder byte stands for the words.
    fn text_unwritten(&mut self, raw: &str) {
        // closing frames look at the real characters: row cleanup trims `|` (possibly an entity), and a
        // list tag made of one non-alphanumeric char is dropped
        let mut ch = raw.trim().chars();
        let lone_symbol = matches!((ch.next(), ch.next()), (Some(c), None) if !c.is_alphanumeric());
        if lone_symbol || raw.bytes().any(|b| b == b'|' || b == b'&') {
            return self.words(raw);
        }
        let lead = skip_ws(raw, 0);
        if lead > 0 {
            self.sp = true;
        }
        if lead == raw.len() {
            return;
        }
        self.flush_text();
        self.md.push('x');
        if raw.ends_with(char::is_whitespace) {
            self.sp = true;
        }
    }

    /// Whether markdown text has to be written right now.
    fn emit(&self) -> bool {
        self.want_md
            || self.head.is_some()
            || self.supsub > 0
            || (self.want_items && !self.thm_stack.is_empty())
            || (self.want_refs && self.bib.is_some())
            || !self.row_stack.is_empty()
            || (self.want_items && self.md.len() < self.item_hi)
    }

    /// Append whitespace-normalised words, one separating space between them.
    fn words(&mut self, t: &str) {
        let n = t.len();
        let b = t.as_bytes();
        let mut i = skip_ws(t, 0);
        if i == n {
            // empty or all whitespace
            if n > 0 {
                self.sp = true;
            }
            return;
        }
        if i > 0 {
            self.sp = true;
        }
        // `from..i` is copied as is while words are separated by one plain space
        let mut from = i;
        i = skip_word(t, i);
        self.flush_text();
        loop {
            let j = skip_ws(t, i);
            if j == n {
                self.md.push_str(&t[from..i]);
                break;
            }
            if j == i + 1 && b[i] == b' ' {
                i = skip_word(t, j);
            } else {
                self.md.push_str(&t[from..i]);
                self.md.push(' ');
                from = j;
                i = skip_word(t, j);
            }
        }
        if i < n {
            self.sp = true;
        }
    }

    fn cap_text(&mut self, t: &str) {
        if t.starts_with(char::is_whitespace) {
            self.cap_sp = true;
        }
        let mut any = false;
        for w in t.split_whitespace() {
            if self.cap_sp && !self.cap.is_empty() && !self.cap.ends_with([' ', '\n']) {
                self.cap.push(' ');
            }
            self.cap.push_str(w);
            self.cap_sp = true;
            any = true;
        }
        if any {
            self.cap_sp = t.ends_with(char::is_whitespace);
        }
    }

    /// Listing / `<pre>` text: keep spaces; newlines only in `<pre>`.
    fn raw_text(&mut self, t: &str) {
        let mut started = false;
        for ch in t.chars() {
            let ch = match ch {
                '\r' => continue,
                '\n' if self.keep_ws == 1 => continue,
                '\u{a0}' => ' ',
                c => c,
            };
            if !started {
                if self.need_break > 0 {
                    self.flush_break();
                }
                self.runin = false;
                self.sp = false;
                started = true;
            }
            self.md.push(ch);
        }
    }

    fn math(&mut self, alt: &str, display: &str, math_start: usize) {
        let block = display == "block" && self.cell_depth == 0 && self.head.is_none();
        let mut dec = std::mem::take(&mut self.dec);
        let mut tex = std::mem::take(&mut self.tex);
        if !self.emit() {
            let t = decoded(alt, &mut dec).trim();
            if !t.is_empty() && !has_displaystyle(t) {
                if block {
                    self.block_break();
                }
                self.flush_text();
                self.md.push('x');
                if block {
                    self.block_break();
                }
                self.dec = dec;
                self.tex = tex;
                return;
            }
        }
        clean_tex(alt, &mut dec, &mut tex);
        if tex.is_empty() {
            // no alttext: fall back to the TeX annotation
            let end = self.s[math_start..].find("</math").map(|e| math_start + e).unwrap_or(self.b.len());
            let inner = &self.s[math_start..end];
            if let Some(k) = inner.find("application/x-tex") {
                if let Some(g) = inner[k..].find('>') {
                    let from = k + g + 1;
                    let to = inner[from..].find("</annotation").map(|e| from + e).unwrap_or(inner.len());
                    clean_tex(&inner[from..to], &mut dec, &mut tex);
                }
            }
        }
        if !tex.is_empty() {
            if block {
                self.block_break();
                self.flush_text();
                self.md.push_str("$$");
                self.md.push_str(&tex);
                self.md.push_str("$$");
                self.block_break();
            } else {
                self.flush_text();
                self.md.push('$');
                self.md.push_str(&tex);
                self.md.push('$');
            }
        }
        self.dec = dec;
        self.tex = tex;
    }

    fn skip_math(&mut self) {
        let from = self.i;
        let at = find_pat(self.b, from, b"</math", &MATH_END_LO).or_else(|| find_pat(self.b, from, b"</MATH", &MATH_END_UP));
        self.i = match at {
            Some(at) => self.s[at..].find('>').map(|g| at + g + 1).unwrap_or(self.b.len()),
            None => self.b.len(),
        };
    }

    // ------------------------------------------------------------------ author block

    fn finish_contact(&mut self, kind: u8, raw: &str) {
        if kind == 0 {
            let from = self.names.len();
            collapse_into(&mut self.names, raw);
            if self.names.len() > from {
                self.names.push('\n');
            }
            return;
        }
        let mut low = std::mem::take(&mut self.dec);
        for line in raw.split('\n') {
            let mut line = strip_emails(line);
            if kind == 2 {
                for name in self.names.split_terminator('\n') {
                    if let Some(rest) = line.trim_start().strip_prefix(name) {
                        if let Some(rest) = rest.trim_start().strip_prefix(',') {
                            line = Cow::Owned(rest.to_string());
                            break;
                        }
                    }
                }
            }
            if kind == 3 {
                for sentence in sentences(&line) {
                    if sentence.len() <= 200 {
                        lower_into(sentence, &mut low);
                        if INST_KW.iter().any(|k| low.contains(k)) && !FUNDING_KW.iter().any(|k| low.contains(k)) {
                            self.add_inst(sentence);
                        }
                    }
                }
            } else {
                self.add_inst(&line);
            }
        }
        self.dec = low;
    }

    fn add_inst(&mut self, raw: &str) {
        // whitespace-collapsed copy of `raw` (never longer than it), on the stack when short
        let mut stack = [0u8; 512];
        let mut heap = String::new();
        let t: &str = if raw.len() <= stack.len() {
            let mut n = 0;
            for (i, w) in raw.split_whitespace().enumerate() {
                if i > 0 {
                    stack[n] = b' ';
                    n += 1;
                }
                stack[n..n + w.len()].copy_from_slice(w.as_bytes());
                n += w.len();
            }
            std::str::from_utf8(&stack[..n]).unwrap_or("")
        } else {
            collapse_into(&mut heap, raw);
            &heap
        };
        // leading footnote marks: "††", "1 ", "* "
        let t0 = t.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '(');
        let mut t1 = t0;
        let mut it = t1.chars();
        if let (Some(d), Some(' ')) = (it.next(), it.next()) {
            if d.is_ascii_digit() {
                t1 = &t1[2..];
            }
        }
        let cleaned = t1.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | ' ' | '-' | '–' | '—'));
        if cleaned.chars().count() >= 2 && !self.insts.iter().any(|i| i == cleaned) {
            self.insts.push(cleaned.to_string());
        }
    }

    // ------------------------------------------------------------------ finish

    fn finish(mut self, head_title: String) -> Doc {
        while self.md.ends_with([' ', '\n']) {
            self.md.pop();
        }
        let mlen = self.md.len();
        for &k in &self.sec_open {
            self.secs[k].end = mlen;
        }
        for s in &mut self.secs {
            s.start = s.start.min(mlen);
            s.end = s.end.clamp(s.start, mlen);
        }
        let title = if self.title.is_empty() { head_title } else { std::mem::take(&mut self.title) };

        let mut built: Vec<(usize, Item)> = Vec::with_capacity(self.items.len());
        for ib in &mut self.items {
            let start = ib.start.min(mlen);
            let end = ib.end.clamp(start, mlen);
            let by_label = label_kind(&ib.label);
            let by_class = kind_word(ib.class_kind);
            let kind = match by_label.or(by_class) {
                Some(KW::Proof) => continue,
                Some(KW::Kind(k)) => k,
                None => Kind::Other,
            };
            let body_start = ib.body_start.clamp(start, end);
            let text = self.md[body_start..end].trim().to_string();
            let label = if ib.label.is_empty() { kind_name(kind).to_string() } else { std::mem::take(&mut ib.label) };
            let sec = ib.sec.map(|k| self.secs[k].id.clone()).unwrap_or_default();
            built.push((start, Item { kind, label, sec, text }));
        }
        built.sort_by_key(|(p, _)| *p);
        Doc {
            title,
            md: self.md,
            sections: self.secs,
            items: built.into_iter().map(|(_, it)| it).collect(),
            refs: self.refs,
        }
    }
}

/// `alt` with entities decoded, in `dec` when there was something to decode.
fn decoded<'b>(alt: &'b str, dec: &'b mut String) -> &'b str {
    if !alt.contains('&') {
        return alt;
    }
    dec.clear();
    decode_into(alt, dec);
    dec.as_str()
}

fn has_displaystyle(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = 0;
    while let Some(k) = find_byte(b, i, b'\\') {
        if b[k..].starts_with(b"\\displaystyle") {
            return true;
        }
        i = k + 1;
    }
    false
}

/// `t` has a newline, tab, double space or `\displaystyle`.
fn needs_clean(t: &str) -> bool {
    let b = t.as_bytes();
    b.iter().enumerate().any(|(i, &c)| match c {
        b'\n' | b'\t' => true,
        b' ' => b.get(i + 1) == Some(&b' '),
        _ => false,
    }) || has_displaystyle(t)
}

/// Cleaned TeX of a math node into `out` (replacing its content): entities decoded, `\displaystyle`
/// dropped (pure noise for an LLM reader), whitespace collapsed.
fn clean_tex(alt: &str, dec: &mut String, out: &mut String) {
    out.clear();
    let t = decoded(alt, dec).trim();
    if !needs_clean(t) {
        out.push_str(t);
        return;
    }
    // words are split at whitespace only, so a word can continue across a dropped `\displaystyle`
    let mut mid_word = false;
    for seg in t.split("\\displaystyle") {
        let lead = seg.starts_with(char::is_whitespace);
        let mut words = seg.split_whitespace();
        let Some(first) = words.next() else {
            if lead {
                mid_word = false;
            }
            continue;
        };
        if !(mid_word && !lead) && !out.is_empty() {
            out.push(' ');
        }
        out.push_str(first);
        for w in words {
            out.push(' ');
            out.push_str(w);
        }
        mid_word = !seg.ends_with(char::is_whitespace);
    }
}

/// "2.1 " -> "2.1", "Appendix A " -> "A", "" -> None.
fn section_id_from_tag(tag: &str) -> Option<String> {
    let last = tag.split_whitespace().last()?;
    let id = last.trim_matches(|c: char| matches!(c, '.' | ':' | '(' | ')'));
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

pub(crate) fn make_ref(fallback_n: u32, tag: &str, text: String, hrefs: &str) -> Ref {
    make_ref_with(fallback_n, tag, text, hrefs, &mut String::new())
}

/// `make_ref` with a caller-owned scratch buffer for the text + links search haystack.
fn make_ref_with(fallback_n: u32, tag: &str, text: String, hrefs: &str, both: &mut String) -> Ref {
    let t = tag.trim();
    let digits = t.trim_matches(|c: char| matches!(c, '[' | ']' | '.' | '(' | ')' | ' '));
    let n = if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse().unwrap_or(fallback_n)
    } else {
        fallback_n
    };
    let hay: &str = if hrefs.is_empty() {
        &text
    } else {
        both.clear();
        both.push_str(&text);
        both.push(' ');
        both.push_str(hrefs);
        both
    };
    let mut arxiv = find_arxiv_id(hay);
    let mut doi = find_doi(hay);
    if let Some(d) = &doi {
        if let Some(id) = d.get(..15).filter(|p| p.eq_ignore_ascii_case("10.48550/arxiv.")).map(|_| &d[15..]) {
            if arxiv.is_none() {
                arxiv = Some(id.to_string());
            }
            doi = None;
        }
    }
    Ref { n, text, arxiv, doi }
}

fn is_new_id(b: &[u8]) -> Option<usize> {
    // \d{4}\.\d{4,5}
    if b.len() < 9 || !b[..4].iter().all(u8::is_ascii_digit) || b[4] != b'.' {
        return None;
    }
    let d = b[5..].iter().take_while(|c| c.is_ascii_digit()).count();
    if (4..=5).contains(&d) {
        Some(5 + d)
    } else {
        None
    }
}

fn is_old_id(b: &[u8]) -> Option<usize> {
    // archive(.XX)?/\d{7}
    let mut k = 0;
    while k < b.len() && (b[k].is_ascii_lowercase() || b[k] == b'-') {
        k += 1;
    }
    if k == 0 {
        return None;
    }
    if k + 3 <= b.len() && b[k] == b'.' && b[k + 1].is_ascii_uppercase() && b[k + 2].is_ascii_uppercase() {
        k += 3;
    }
    if k < b.len() && b[k] == b'/' && b.len() >= k + 8 && b[k + 1..k + 8].iter().all(u8::is_ascii_digit) {
        Some(k + 8)
    } else {
        None
    }
}

fn find_arxiv_id(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let lower_hit = |from: usize| -> Option<usize> {
        // case-insensitive "arxiv" or "abs/" anchors
        let mut i = from;
        while i + 4 <= b.len() {
            if b[i].eq_ignore_ascii_case(&b'a') {
                if i + 5 <= b.len() && b[i..i + 5].eq_ignore_ascii_case(b"arxiv") {
                    return Some(i + 5);
                }
                if b[i..i + 4].eq_ignore_ascii_case(b"abs/") && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
                    return Some(i + 4);
                }
            }
            i += 1;
        }
        None
    };
    let mut from = 0;
    while let Some(after) = lower_hit(from) {
        from = after;
        let mut j = after;
        // skip ".org/abs/", ".org/pdf/", ":", whitespace
        loop {
            let rest = &s[j..];
            if let Some(r) = rest.strip_prefix(".org/") {
                j += s[j..].len() - r.len();
                let r2 = &s[j..];
                if r2.starts_with("abs/") || r2.starts_with("pdf/") {
                    j += 4;
                } else if r2.starts_with("html/") {
                    j += 5;
                }
                continue;
            }
            match b.get(j) {
                Some(b':') | Some(b' ') | Some(b'/') | Some(b'=') => j += 1,
                _ => break,
            }
        }
        let rest = &b[j..];
        let len = is_new_id(rest).or_else(|| is_old_id(rest));
        if let Some(l) = len {
            return Some(s[j..j + l].to_string());
        }
    }
    None
}

fn find_doi(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(p) = s[from..].find("10.") {
        let a = from + p;
        from = a + 3;
        if a > 0 && b[a - 1].is_ascii_alphanumeric() {
            continue;
        }
        let digits = b[a + 3..].iter().take_while(|c| c.is_ascii_digit()).count();
        if !(4..=9).contains(&digits) || b.get(a + 3 + digits) != Some(&b'/') {
            continue;
        }
        let end = s[a..].find(|c: char| c.is_whitespace() || c == '"').map(|e| a + e).unwrap_or(s.len());
        let mut d = &s[a..end];
        // trailing punctuation goes, but balanced brackets belong to the DOI: 10.1061/(ASCE)0733-9410(1990)116:8(1231)
        while let Some(c) = d.chars().next_back() {
            let unmatched = |open: char, close: char| d.matches(close).count() > d.matches(open).count();
            if matches!(c, '.' | ',' | ';') || (c == ')' && unmatched('(', ')')) || (c == ']' && unmatched('[', ']')) {
                d = &d[..d.len() - 1];
            } else {
                break;
            }
        }
        if d.len() > 3 + digits + 1 {
            return Some(d.to_string());
        }
    }
    None
}

/// Remove e-mail addresses (and `{a, b}@dom` groups) from a line.
fn strip_emails(line: &str) -> Cow<'_, str> {
    if !line.contains('@') {
        return Cow::Borrowed(line);
    }
    let mut s = line.to_string();
    while let Some(at) = s.find('@') {
        let mut l = s[..at].rfind(char::is_whitespace).map(|p| p + 1).unwrap_or(0);
        if s[..at].ends_with('}') {
            if let Some(o) = s[..at].rfind('{') {
                l = s[..o].rfind(char::is_whitespace).map(|p| p + 1).unwrap_or(0).min(o);
            }
        }
        let r = s[at..].find(char::is_whitespace).map(|p| at + p).unwrap_or(s.len());
        s.replace_range(l..r, " ");
    }
    // "E-mail:" style labels left behind
    let mut out = String::new();
    for w in s.split_whitespace() {
        let label = ["e-mail:", "email:", "e-mail", "email", "emails:"].iter().any(|l| w.eq_ignore_ascii_case(l));
        if label || w.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("http")) {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    Cow::Owned(out)
}

/// Sentences of `s`: split after a `.` that follows a non-digit and precedes ` ` + an uppercase letter.
fn sentences(s: &str) -> impl Iterator<Item = &str> {
    let mut start = 0;
    let mut prev: Option<char> = None;
    let mut it = s.char_indices();
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        while let Some((i, c)) = it.next() {
            if c == '.' && prev.is_some_and(|p| !p.is_ascii_digit()) && s[i + 1..].starts_with(' ') {
                let after = &s[i + 2..];
                if after.chars().next().is_some_and(char::is_uppercase) {
                    let sentence = &s[start..i + 1];
                    start = i + 2;
                    it.next();
                    prev = Some(' ');
                    return Some(sentence);
                }
            }
            prev = Some(c);
        }
        done = true;
        let rest = &s[start..];
        (!rest.trim().is_empty()).then_some(rest)
    })
}

const INST_KW: [&str; 17] = [
    "univers", "institut", "college", "school of", "department", "faculty", "laborator", "academy", "center",
    "centre", "polytechn", "hospital", "max planck", "cnrs", "inria", "fakult", "dept",
];

const FUNDING_KW: [&str; 10] =
    ["supported", "funded", "grant", "partially", "acknowledg", "equal contribution", "work done", "correspond", "thank", "fellowship"];

/// `out` = `s.to_lowercase()` (without allocating for ASCII).
fn lower_into(s: &str, out: &mut String) {
    out.clear();
    if s.is_ascii() {
        out.extend(s.bytes().map(|b| b.to_ascii_lowercase() as char));
    } else {
        out.push_str(&s.to_lowercase());
    }
}

// ---------------------------------------------------------------------------------------------
// kinds
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum KW {
    Kind(Kind),
    Proof,
}

/// Kind named by a printed label: the first recognised word, so qualifiers ("Main Conjecture",
/// "Open problem", "Key Lemma") do not hide it.
fn label_kind(label: &str) -> Option<KW> {
    label.split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()).find_map(kind_word)
}

fn kind_word(word: &str) -> Option<KW> {
    let w = word.to_ascii_lowercase();
    Some(match w.as_str() {
        "theorem" | "thm" | "theor" => KW::Kind(Kind::Theorem),
        "lemma" | "lem" | "lemme" => KW::Kind(Kind::Lemma),
        "proposition" | "prop" => KW::Kind(Kind::Proposition),
        "corollary" | "cor" | "coro" | "corol" => KW::Kind(Kind::Corollary),
        "definition" | "defn" | "def" | "dfn" | "defi" | "deff" | "defin" => KW::Kind(Kind::Definition),
        "remark" | "rem" | "rmk" | "rk" | "remk" => KW::Kind(Kind::Remark),
        "example" | "exm" | "exmp" | "exa" | "ex" | "examp" => KW::Kind(Kind::Example),
        "exercise" | "exer" | "exc" | "exo" => KW::Kind(Kind::Exercise),
        "conjecture" | "conj" | "conjec" => KW::Kind(Kind::Conjecture),
        "problem" | "prob" | "pb" | "problems" => KW::Kind(Kind::Problem),
        "question" | "ques" | "quest" | "qn" | "q" => KW::Kind(Kind::Question),
        "claim" | "cl" => KW::Kind(Kind::Claim),
        "proof" | "pf" => KW::Proof,
        _ => return None,
    })
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Problem => "Problem",
        Kind::Conjecture => "Conjecture",
        Kind::Question => "Question",
        Kind::Exercise => "Exercise",
        Kind::Theorem => "Theorem",
        Kind::Lemma => "Lemma",
        Kind::Proposition => "Proposition",
        Kind::Corollary => "Corollary",
        Kind::Definition => "Definition",
        Kind::Remark => "Remark",
        Kind::Example => "Example",
        Kind::Claim => "Claim",
        Kind::Other => "Item",
    }
}

// ---------------------------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------------------------

/// Innermost section containing byte `pos` ("" if none).
fn sec_at(secs: &[Section], pos: usize) -> &str {
    let mut k = secs.partition_point(|s| s.start <= pos);
    while k > 0 {
        k -= 1;
        if pos < secs[k].end {
            return &secs[k].id;
        }
    }
    ""
}

fn snippet(text: &str, pos: usize, mlen: usize, ctx: usize) -> String {
    let n = text.len();
    let pos = floor_boundary(text, pos.min(n));
    let mut s = pos;
    for _ in 0..ctx {
        if s == 0 {
            break;
        }
        s -= 1;
        while !text.is_char_boundary(s) {
            s -= 1;
        }
    }
    let cap = ctx.saturating_mul(4).saturating_add(100);
    let mut e = ceil_boundary(text, pos.saturating_add(mlen.min(cap)).min(n));
    let mut cut = mlen > cap;
    for _ in 0..ctx {
        if e >= n {
            break;
        }
        e += text[e..].chars().next().map(char::len_utf8).unwrap_or(1);
    }
    if e < n {
        cut = true;
    }
    let mut out = String::with_capacity(e - s + 8);
    if s > 0 {
        out.push('…');
    }
    for ch in text[s..e].chars() {
        out.push(if ch == '\n' || ch == '\r' { ' ' } else { ch });
    }
    if cut {
        out.push('…');
    }
    out
}

fn literal_find_all(text: &str, pat: &str, push: &mut dyn FnMut(usize, usize) -> bool) {
    let tb = text.as_bytes();
    if pat.is_ascii() {
        let pb = pat.as_bytes();
        let m = pb.len();
        let lo = pb[0].to_ascii_lowercase();
        let up = pb[0].to_ascii_uppercase();
        let mut i = 0;
        while i + m <= tb.len() {
            let Some(off) = tb[i..tb.len() - m + 1].iter().position(|&b| b == lo || b == up) else { break };
            let at = i + off;
            if tb[at..at + m].eq_ignore_ascii_case(pb) {
                if !push(at, m) {
                    return;
                }
                i = at + m;
            } else {
                i = at + 1;
            }
        }
        return;
    }
    let pc: Vec<char> = pat.chars().map(lower1).collect();
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap_or('\0');
        let clen = c.len_utf8().max(1);
        if lower1(c) == pc[0] {
            let mut end = i + clen;
            let mut ok = true;
            let mut sub = text[end..].chars();
            for &p in &pc[1..] {
                match sub.next() {
                    Some(tc) if lower1(tc) == p => end += tc.len_utf8(),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if !push(i, end - i) {
                    return;
                }
                i = end;
                continue;
            }
        }
        i += clen;
    }
}

fn lower1(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// `/re/` or `/re/i` -> (re, case_insensitive).
fn regex_form(pat: &str) -> Option<(&str, bool)> {
    if pat.len() >= 2 && pat.starts_with('/') {
        if pat.ends_with('/') {
            return Some((&pat[1..pat.len() - 1], false));
        }
        if pat.ends_with("/i") && pat.len() >= 3 {
            return Some((&pat[1..pat.len() - 2], true));
        }
    }
    None
}

#[derive(Debug)]
enum SetItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Debug)]
enum Node {
    Lit(char),
    Any,
    Set { neg: bool, items: Vec<SetItem> },
    Bol,
    Eol,
    WordB(bool),
    Group(Vec<Vec<Node>>),
    Rep { node: Box<Node>, min: u32, max: u32, lazy: bool },
}

struct Regex {
    alts: Vec<Vec<Node>>,
    ci: bool,
    /// Bytes a non-empty match can start with (lead bytes only for non-ASCII); `None` = no filtering.
    first: Option<Box<[bool; 256]>>,
}

struct RxParser {
    c: Vec<char>,
    i: usize,
}

impl RxParser {
    fn alts(&mut self, depth: u32) -> Option<Vec<Vec<Node>>> {
        if depth > 64 {
            return None;
        }
        let mut alts = vec![Vec::new()];
        while self.i < self.c.len() {
            let ch = self.c[self.i];
            match ch {
                ')' => {
                    if depth == 0 {
                        return None;
                    }
                    break;
                }
                '|' => {
                    self.i += 1;
                    alts.push(Vec::new());
                    continue;
                }
                _ => {}
            }
            let atom = self.atom(depth)?;
            let atom = self.quant(atom)?;
            alts.last_mut()?.push(atom);
        }
        Some(alts)
    }

    fn atom(&mut self, depth: u32) -> Option<Node> {
        let ch = self.c[self.i];
        self.i += 1;
        Some(match ch {
            '.' => Node::Any,
            '^' => Node::Bol,
            '$' => Node::Eol,
            '(' => {
                if self.c[self.i..].starts_with(&['?', ':']) {
                    self.i += 2;
                }
                let g = self.alts(depth + 1)?;
                if self.i >= self.c.len() || self.c[self.i] != ')' {
                    return None;
                }
                self.i += 1;
                simplify_group(g)
            }
            '[' => self.set()?,
            '\\' => {
                let e = *self.c.get(self.i)?;
                self.i += 1;
                match e {
                    'd' => Node::Set { neg: false, items: vec![SetItem::Digit(true)] },
                    'D' => Node::Set { neg: false, items: vec![SetItem::Digit(false)] },
                    'w' => Node::Set { neg: false, items: vec![SetItem::Word(true)] },
                    'W' => Node::Set { neg: false, items: vec![SetItem::Word(false)] },
                    's' => Node::Set { neg: false, items: vec![SetItem::Space(true)] },
                    'S' => Node::Set { neg: false, items: vec![SetItem::Space(false)] },
                    'b' => Node::WordB(true),
                    'B' => Node::WordB(false),
                    'n' => Node::Lit('\n'),
                    't' => Node::Lit('\t'),
                    c => Node::Lit(c),
                }
            }
            '*' | '+' | '?' => return None,
            c => Node::Lit(c),
        })
    }

    fn set(&mut self) -> Option<Node> {
        let mut neg = false;
        if self.c.get(self.i) == Some(&'^') {
            neg = true;
            self.i += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let ch = *self.c.get(self.i)?;
            self.i += 1;
            if ch == ']' && !first {
                break;
            }
            first = false;
            let lo = if ch == '\\' {
                let e = *self.c.get(self.i)?;
                self.i += 1;
                match e {
                    'd' => {
                        items.push(SetItem::Digit(true));
                        continue;
                    }
                    'D' => {
                        items.push(SetItem::Digit(false));
                        continue;
                    }
                    'w' => {
                        items.push(SetItem::Word(true));
                        continue;
                    }
                    'W' => {
                        items.push(SetItem::Word(false));
                        continue;
                    }
                    's' => {
                        items.push(SetItem::Space(true));
                        continue;
                    }
                    'S' => {
                        items.push(SetItem::Space(false));
                        continue;
                    }
                    'n' => '\n',
                    't' => '\t',
                    c => c,
                }
            } else {
                ch
            };
            if self.c.get(self.i) == Some(&'-') && self.c.get(self.i + 1).is_some_and(|c| *c != ']') {
                self.i += 1;
                let mut hi = self.c[self.i];
                self.i += 1;
                if hi == '\\' {
                    hi = *self.c.get(self.i)?;
                    self.i += 1;
                }
                if hi < lo {
                    return None;
                }
                items.push(SetItem::Range(lo, hi));
            } else {
                items.push(SetItem::Range(lo, lo));
            }
        }
        Some(Node::Set { neg, items })
    }

    fn quant(&mut self, atom: Node) -> Option<Node> {
        let (min, max) = match self.c.get(self.i) {
            Some('*') => (0, u32::MAX),
            Some('+') => (1, u32::MAX),
            Some('?') => (0, 1),
            Some('{') => {
                // {n}, {n,}, {n,m}; anything else is a literal brace
                let Some(close) = self.c[self.i..].iter().position(|&c| c == '}').map(|p| p + self.i) else {
                    return Some(atom);
                };
                let body: String = self.c[self.i + 1..close].iter().collect();
                let parsed = match body.split_once(',') {
                    Some((a, b)) => match (a.trim().parse::<u32>(), b.trim()) {
                        (Ok(a), "") => Some((a, u32::MAX)),
                        (Ok(a), b) => b.parse::<u32>().ok().map(|b| (a, b)),
                        _ => None,
                    },
                    None => body.trim().parse::<u32>().ok().map(|v| (v, v)),
                };
                match parsed {
                    Some((a, b)) if a <= b && (b == u32::MAX || b <= 1000) => {
                        self.i = close;
                        (a, b)
                    }
                    _ => return Some(atom),
                }
            }
            _ => return Some(atom),
        };
        self.i += 1;
        if matches!(atom, Node::Bol | Node::Eol | Node::WordB(_)) {
            return None;
        }
        let lazy = if self.c.get(self.i) == Some(&'?') {
            self.i += 1;
            true
        } else {
            false
        };
        Some(Node::Rep { node: Box::new(atom), min, max, lazy })
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

struct Matcher<'t> {
    text: &'t str,
    ci: bool,
    steps: Cell<u64>,
    /// A step/recursion budget ran out somewhere: absence of a match is then not trustworthy.
    blown: Cell<bool>,
}

/// Backtracking budget per start position / per grep call; exhausting it ends the match attempt.
const STEP_BUDGET: u64 = 100_000;
const TOTAL_BUDGET: u64 = 20_000_000;

impl Regex {
    fn compile(src: &str, ci: bool) -> Option<Regex> {
        if src.is_empty() {
            return None;
        }
        let mut p = RxParser { c: src.chars().collect(), i: 0 };
        let alts = p.alts(0)?;
        if p.i != p.c.len() {
            return None;
        }
        let mut first = Box::new([false; 256]);
        let known = seq_alts_first(&alts, ci, &mut first);
        Some(Regex { alts, ci, first: known.then_some(first) })
    }

    /// Returns true when a budget was exhausted during the scan.
    fn find_all(&self, text: &str, push: &mut dyn FnMut(usize, usize) -> bool) -> bool {
        let m = Matcher { text, ci: self.ci, steps: Cell::new(0), blown: Cell::new(false) };
        // literal first-char acceleration
        let first_lit = match self.alts.as_slice() {
            [alt] => match alt.first() {
                Some(Node::Lit(c)) if !self.ci => Some(*c),
                _ => None,
            },
            _ => None,
        };
        let mut pos = 0usize;
        let n = text.len();
        let mut total = 0u64;
        let tb = text.as_bytes();
        while pos <= n {
            if let Some(c) = first_lit {
                match text[pos..].find(c) {
                    Some(o) => pos += o,
                    None => return m.blown.get(),
                }
            } else if let Some(first) = &self.first {
                // positions that cannot start a match are skipped, and are not charged to the total budget
                match tb[pos..].iter().position(|&b| first[b as usize]) {
                    Some(o) => pos += o,
                    None => return m.blown.get(),
                }
            }
            if total > TOTAL_BUDGET {
                m.blown.set(true);
                return true;
            }
            m.steps.set(0);
            let end = Cell::new(usize::MAX);
            let ok = m.alts(&self.alts, pos, &|p| {
                end.set(p);
                true
            });
            total += m.steps.get().max(1);
            if ok && end.get() > pos {
                if !push(pos, end.get() - pos) {
                    return m.blown.get();
                }
                pos = end.get();
                continue;
            }
            if pos >= n {
                break;
            }
            pos += text[pos..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
        m.blown.get()
    }
}

/// Recursion cap for grouped repetition (worker threads have small stacks).
const MAX_REP_DEPTH: u32 = 20_000;

/// `(x)` / `(a|[b-c])` made of single-character nodes become one node, which the matcher repeats
/// iteratively instead of recursing per repetition.
fn simplify_group(mut g: Vec<Vec<Node>>) -> Node {
    let one = |n: &Node| matches!(n, Node::Lit(_) | Node::Any | Node::Set { .. });
    if g.len() == 1 && g[0].len() == 1 && one(&g[0][0]) {
        return g.pop().and_then(|mut a| a.pop()).unwrap();
    }
    if g.len() > 1 && g.iter().all(|a| a.len() == 1 && matches!(&a[0], Node::Lit(_) | Node::Set { neg: false, .. })) {
        let mut items = Vec::new();
        for a in g {
            match a.into_iter().next().unwrap() {
                Node::Lit(c) => items.push(SetItem::Range(c, c)),
                Node::Set { items: it, .. } => items.extend(it),
                _ => unreachable!(),
            }
        }
        return Node::Set { neg: false, items };
    }
    Node::Group(g)
}

fn set_hit(neg: bool, items: &[SetItem], ci: bool, c: char) -> bool {
    let hit = |ch: char| {
        items.iter().any(|it| match it {
            SetItem::Range(a, b) => *a <= ch && ch <= *b,
            SetItem::Digit(p) => ch.is_ascii_digit() == *p,
            SetItem::Word(p) => is_word(ch) == *p,
            SetItem::Space(p) => ch.is_whitespace() == *p,
        })
    };
    let mut h = hit(c);
    if !h && ci {
        h = hit(lower1(c)) || c.to_uppercase().next().is_some_and(&hit);
    }
    h != neg
}

/// Union of the bytes a non-empty match of `alts` can start with. Returns false when that cannot be
/// bounded (case-insensitive non-ASCII literal).
fn seq_alts_first(alts: &[Vec<Node>], ci: bool, set: &mut [bool; 256]) -> bool {
    alts.iter().all(|a| seq_first(a, ci, set).is_some())
}

/// Adds the possible first bytes of `nodes` to `set`; `Some(nullable)`, or `None` when unbounded.
fn seq_first(nodes: &[Node], ci: bool, set: &mut [bool; 256]) -> Option<bool> {
    for n in nodes {
        if !node_first(n, ci, set)? {
            return Some(false);
        }
    }
    Some(true)
}

fn node_first(n: &Node, ci: bool, set: &mut [bool; 256]) -> Option<bool> {
    let all_lead = |set: &mut [bool; 256]| set[0xC0..].fill(true);
    match n {
        Node::Lit(c) => {
            if ci {
                if !c.is_ascii() {
                    return None;
                }
                set[c.to_ascii_lowercase() as usize] = true;
                set[c.to_ascii_uppercase() as usize] = true;
                all_lead(set);
            } else {
                let mut b = [0u8; 4];
                set[c.encode_utf8(&mut b).as_bytes()[0] as usize] = true;
            }
            Some(false)
        }
        Node::Any => {
            for (b, v) in set.iter_mut().enumerate().take(0x80) {
                *v |= b != b'\n' as usize;
            }
            all_lead(set);
            Some(false)
        }
        Node::Set { neg, items } => {
            for b in 0..0x80u8 {
                if set_hit(*neg, items, ci, b as char) {
                    set[b as usize] = true;
                }
            }
            all_lead(set);
            Some(false)
        }
        Node::Bol | Node::Eol | Node::WordB(_) => Some(true),
        Node::Group(alts) => {
            let mut nullable = false;
            for a in alts {
                nullable |= seq_first(a, ci, set)?;
            }
            Some(nullable)
        }
        Node::Rep { node, min, .. } => Some(node_first(node, ci, set)? || *min == 0),
    }
}

impl Matcher<'_> {
    fn bump(&self) -> bool {
        let s = self.steps.get() + 1;
        self.steps.set(s);
        if s > STEP_BUDGET {
            self.blown.set(true);
            return true;
        }
        false
    }

    fn char_at(&self, pos: usize) -> Option<char> {
        self.text.get(pos..).and_then(|r| r.chars().next())
    }

    fn alts(&self, alts: &[Vec<Node>], pos: usize, k: &dyn Fn(usize) -> bool) -> bool {
        alts.iter().any(|a| self.seq(a, pos, k))
    }

    fn eq(&self, a: char, b: char) -> bool {
        a == b || (self.ci && lower1(a) == lower1(b))
    }

    /// Single-char node match: Some(next_pos).
    fn one(&self, node: &Node, pos: usize) -> Option<usize> {
        let c = self.char_at(pos)?;
        let ok = match node {
            Node::Lit(l) => self.eq(c, *l),
            Node::Any => c != '\n',
            Node::Set { neg, items } => set_hit(*neg, items, self.ci, c),
            _ => return None,
        };
        if ok {
            Some(pos + c.len_utf8())
        } else {
            None
        }
    }

    fn seq(&self, nodes: &[Node], pos: usize, k: &dyn Fn(usize) -> bool) -> bool {
        if self.bump() {
            return false;
        }
        let Some((first, rest)) = nodes.split_first() else { return k(pos) };
        match first {
            Node::Lit(_) | Node::Any | Node::Set { .. } => match self.one(first, pos) {
                Some(np) => self.seq(rest, np, k),
                None => false,
            },
            Node::Bol => (pos == 0 || self.text.as_bytes()[pos - 1] == b'\n') && self.seq(rest, pos, k),
            Node::Eol => (pos == self.text.len() || self.text.as_bytes()[pos] == b'\n') && self.seq(rest, pos, k),
            Node::WordB(want) => {
                let before = self.text[..pos].chars().next_back().is_some_and(is_word);
                let after = self.char_at(pos).is_some_and(is_word);
                ((before != after) == *want) && self.seq(rest, pos, k)
            }
            Node::Group(alts) => alts.iter().any(|a| self.seq(a, pos, &|p| self.seq(rest, p, k))),
            Node::Rep { node, min, max, lazy } => {
                if matches!(**node, Node::Lit(_) | Node::Any | Node::Set { .. }) {
                    self.rep_simple(node, *min, *max, *lazy, pos, rest, k)
                } else {
                    self.rep_general(node, *min, *max, *lazy, 0, pos, rest, k)
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rep_simple(&self, node: &Node, min: u32, max: u32, lazy: bool, pos: usize, rest: &[Node], k: &dyn Fn(usize) -> bool) -> bool {
        if lazy {
            let mut p = pos;
            let mut cnt = 0u32;
            loop {
                if cnt >= min && self.seq(rest, p, k) {
                    return true;
                }
                if cnt >= max || self.bump() {
                    return false;
                }
                match self.one(node, p) {
                    Some(np) => {
                        p = np;
                        cnt += 1;
                    }
                    None => return false,
                }
            }
        }
        let mut positions_end = pos;
        let mut cnt = 0u32;
        while cnt < max {
            match self.one(node, positions_end) {
                Some(np) => {
                    positions_end = np;
                    cnt += 1;
                }
                None => break,
            }
            if self.steps.get() > STEP_BUDGET {
                return false;
            }
        }
        let mut p = positions_end;
        loop {
            if cnt < min {
                return false;
            }
            if self.seq(rest, p, k) {
                return true;
            }
            if cnt == 0 || cnt == min {
                return false;
            }
            // step back one char
            let prev = self.text[..p].chars().next_back().map(char::len_utf8).unwrap_or(0);
            p -= prev;
            cnt -= 1;
            if self.bump() {
                return false;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rep_general(&self, node: &Node, min: u32, max: u32, lazy: bool, count: u32, pos: usize, rest: &[Node], k: &dyn Fn(usize) -> bool) -> bool {
        if self.bump() {
            return false;
        }
        if count > MAX_REP_DEPTH {
            self.blown.set(true);
            return false;
        }
        let more = |this: &Self| -> bool {
            count < max
                && this.seq(std::slice::from_ref(node), pos, &|p| {
                    if p == pos && count >= min {
                        return false;
                    }
                    this.rep_general(node, min, max, lazy, count + 1, p, rest, k)
                })
        };
        let done = |this: &Self| count >= min && this.seq(rest, pos, k);
        if lazy {
            done(self) || more(self)
        } else {
            more(self) || done(self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name))
            .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    fn labels(d: &Doc) -> Vec<String> {
        d.items.iter().map(|i| i.label.clone()).collect()
    }

    fn sec_ids(d: &Doc) -> Vec<String> {
        d.sections.iter().map(|s| s.id.clone()).collect()
    }

    fn doc(body: &str) -> Doc {
        parse(&format!("<html><body><article class=\"ltx_document\">{body}</article></body></html>"))
    }

    // ----------------------------------------------------------------------- real pages

    #[test]
    fn anticodes_structure() {
        let d = parse(&fx("html_math_anticodes.html"));
        assert_eq!(d.title, "The Asymptotic Number of Anticodes in Hamming Spaces");
        assert_eq!(sec_ids(&d), ["s1", "1", "5", "5.1", "5.2", "5.3", "s2"]);
        let levels: Vec<u8> = d.sections.iter().map(|s| s.level).collect();
        assert_eq!(levels, [1, 1, 1, 2, 2, 2, 1]);
        assert_eq!(d.sections[2].title, "Conclusion, further discussion, and open problems");
        assert_eq!(d.sections[4].title, "Asymptotics for growing $q$");
        assert_eq!(insts_only(&fx("html_math_anticodes.html")), ["University of St.Gallen, Switzerland", "Delft University of Technology, Netherlands"]);
        assert_eq!(labels(&d), ["Theorem 3.11", "Theorem 4.6", "Problem 5.1", "Problem 5.2"]);
        let kinds: Vec<Kind> = d.items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, [Kind::Theorem, Kind::Theorem, Kind::Problem, Kind::Problem]);
        let p = &d.items[2];
        assert_eq!(p.sec, "5.2");
        assert!(p.text.starts_with("Determine the asymptotic behavior for $|\\mathcal{A}_{q(n)}(n,d(n))|$ and"), "{}", p.text);
        assert_eq!(d.items[3].sec, "5.3");
        assert!(d.md.contains("**Problem 5.1.** Determine the asymptotic behavior"));
        // innerthm restated theorem: kind comes from the printed label, not `ltx_theorem_innerthm`
        assert_eq!(d.items[0].sec, "1");
    }

    #[test]
    fn anticodes_refs() {
        let d = parse(&fx("html_math_anticodes.html"));
        assert_eq!(d.refs.len(), 17);
        assert_eq!(d.refs[0].n, 1);
        assert!(d.refs[0].text.starts_with("R. Ahlswede and G. O. H. Katona, Contributions to the geometry of Hamming spaces"));
        assert_eq!(d.refs[3].arxiv.as_deref(), Some("2405.10275"));
        assert_eq!(d.refs[0].arxiv, None);
    }

    #[test]
    fn bruhat_theorems_and_citations() {
        let d = parse(&fx("html_math_bruhat.html"));
        assert_eq!(sec_ids(&d), ["s1", "1", "2", "8", "s2", "s3"]);
        assert_eq!(d.sections[4].level, 3);
        assert_eq!(
            labels(&d),
            [
                "Proposition 1.1",
                "Proposition 1.2",
                "Proposition 1.4",
                "Lemma 1.6",
                "Theorem 2.1",
                "Theorem 2.2",
                "Theorem 2.3"
            ]
        );
        // proofs are not items, but stay in the text
        assert!(d.md.contains("**Proof 1.3.** All four parts follow easily from Proposition 1.1."));
        // citation link markup dropped, `[key]` text kept
        assert!(d.md.contains("such as [BjoBre05], [Davis25], [Schrem23], [Luszti14], [GecPfi00] for definitions"));
        assert!(d.md.contains("[Luszti14, §2.1]"));
        assert!(d.md.contains("conjectured [fn: in https://mathoverflow.net/questions/472072/] that"));
        // display math from alttext
        assert!(d.md.contains("$u,v\\in W$"));
        assert_eq!(d.items[1].text.lines().next().unwrap(), "Let $u,v\\in W$. Then:");
        assert!(d.items[1].text.contains("\n- (a) If $u\\leq v$, then $\\ell\\left(u\\right)\\leq\\ell\\left(v\\right)$"), "{}", d.items[1].text);
        assert_eq!(d.refs.len(), 18);
        assert_eq!(d.refs[0].doi.as_deref(), Some("10.1007/3-540-27596-7"));
        assert_eq!(d.refs[1].doi.as_deref(), Some("10.37236/6741"));
        assert_eq!(d.refs[2].doi, None);
        assert_eq!(d.refs[1].n, 2);

    }

    #[test]
    fn chapters_contain_their_sections() {
        let d = doc(r#"<section class="ltx_chapter" id="Ch1"><h2 class="ltx_title ltx_title_chapter"><span class="ltx_tag ltx_tag_chapter">1</span>Intro</h2>
<section class="ltx_section" id="S1"><h3 class="ltx_title ltx_title_section"><span class="ltx_tag ltx_tag_section">1.1</span>First</h3><p class="ltx_p">Alpha.</p></section>
<section class="ltx_section" id="S2"><h3 class="ltx_title ltx_title_section"><span class="ltx_tag ltx_tag_section">1.2</span>Second</h3><p class="ltx_p">Beta.</p></section></section>
<section class="ltx_chapter" id="Ch2"><h2 class="ltx_title ltx_title_chapter"><span class="ltx_tag ltx_tag_chapter">2</span>Next</h2><p class="ltx_p">Gamma.</p></section>"#);
        assert_eq!(d.sections.iter().map(|s| (s.id.as_str(), s.level)).collect::<Vec<_>>(), [("1", 1), ("1.1", 2), ("1.2", 2), ("2", 1)]);
        let ch1 = &d.md[d.sections[0].start..d.sections[0].end];
        assert!(ch1.contains("Alpha.") && ch1.contains("Beta.") && !ch1.contains("Gamma."), "{ch1}");
    }

    #[test]
    fn chapter_documents_keep_bibliography_at_chapter_level() {
        let d = doc(r#"<section class="ltx_chapter"><h2 class="ltx_title ltx_title_chapter">Chapter</h2><p class="ltx_p">Body.</p></section>
<section class="ltx_bibliography"><h2 class="ltx_title ltx_title_bibliography">References</h2><p class="ltx_p">Citation.</p></section>"#);
        assert_eq!(d.sections.iter().map(|s| s.level).collect::<Vec<_>>(), [1, 1]);
        assert!(!d.md[d.sections[0].start..d.sections[0].end].contains("Citation."));
    }

    #[test]
    fn qualified_labels_keep_their_kind() {
        let d = doc(r#"<div class="ltx_theorem ltx_theorem_conjecture"><h6 class="ltx_title ltx_title_theorem">Main Conjecture.</h6><p class="ltx_p">Every such group is finite.</p></div>
<div class="ltx_theorem ltx_theorem_problem"><h6 class="ltx_title ltx_title_theorem">Open Problem 3.</h6><p class="ltx_p">Is it true?</p></div>"#);
        assert_eq!(d.items.iter().map(|i| i.kind).collect::<Vec<_>>(), [Kind::Conjecture, Kind::Problem]);
    }

    #[test]
    fn section_ranges_nest() {
        let d = parse(&fx("html_math_anticodes.html"));
        let s5 = find_section(&d.sections, "5").unwrap();
        let s52 = find_section(&d.sections, "5.2").unwrap();
        assert!(s5.start <= s52.start && s52.end <= s5.end);
        assert!(d.md[s52.start..s52.end].starts_with("### 5.2 Asymptotics for growing $q$"));
        assert!(d.md[s5.start..s5.end].contains("### 5.3 Restricted distances"));
        let refs = find_section(&d.sections, "references").unwrap();
        assert!(d.md[refs.start..refs.end].starts_with("## References"));
        assert_eq!(refs.end, d.md.len());
        // siblings do not overlap
        for w in d.sections.windows(2) {
            if w[1].level <= w[0].level {
                assert!(w[0].end <= w[1].start);
            }
        }
    }

    #[test]
    fn problem_theorem_env_and_odd_kinds() {
        let d = parse(&fx("html_problem_env.html"));
        assert_eq!(d.title, "Features of a high school olympiad problem");
        assert_eq!(
            labels(&d),
            ["Problem 1.1", "Properties 4.1", "Instructions 7.1", "Theorem 7.2", "Theorem 8.1", "Theorem 8.2", "Example 8.3"]
        );
        assert_eq!(d.items[0].kind, Kind::Problem);
        assert_eq!(d.items[0].sec, "1");
        assert_eq!(d.items[1].kind, Kind::Other);
        assert_eq!(d.items[6].kind, Kind::Example);
        assert_eq!(d.items[3].sec, "7");
        assert_eq!(d.refs.len(), 6);
        // a Problem(1.1) reference inside running text is not an item
        assert_eq!(d.items.iter().filter(|i| i.kind == Kind::Problem).count(), 1);
    }

    #[test]
    fn ar5iv_page() {
        let d = parse(&fx("html_ar5iv.html"));
        assert_eq!(d.title, "Attention Is All You Need");
        assert_eq!(insts_only(&fx("html_ar5iv.html")), ["Google Brain", "Google Research", "University of Toronto"]);
        assert_eq!(
            sec_ids(&d),
            ["s1", "1", "3", "3.1", "3.2", "3.2.1", "3.2.2", "3.2.3", "3.3", "3.4", "3.5", "4", "5", "5.1", "5.2", "5.3", "5.4", "s2"]
        );
        assert_eq!(d.sections[5].level, 3);
        assert_eq!(d.refs.len(), 40);
        assert_eq!(d.refs[0].n, 1);
        assert_eq!(d.refs[0].arxiv.as_deref(), Some("1607.06450"));
        assert_eq!(d.refs[1].arxiv.as_deref(), Some("1409.0473"));
        assert_eq!(d.refs[7].arxiv, None);
        assert!(d.md.contains("[1]"));
        assert!(d.md.contains("$\\mathrm{Attention}(Q,K,V)=\\mathrm{softmax}(\\frac{QK^{T}}{\\sqrt{d_{k}}})V$") || d.md.contains("$$"));
    }

    #[test]
    fn nemotron_tables_footnotes_figures() {
        let d = parse(&fx("html_llm_nemotron.html"));
        assert_eq!(d.title, "An Open Recipe for IMO Gold: Training Nemotron for Olympiad Mathematics");
        assert!(d.md.contains("[fn: See the IMO Grand Challenge and AIMO Prize initiatives.]"));
        assert!(d.md.contains("\nP1 | 7 | R1 | 5.57M | 218M | 11.7 | 459.2\n"));
        assert!(d.md.contains("\nCompetition total | 30 | – | 707M | 2.31B | 1,464.4 | 4,784.6\n"));
        assert!(d.md.contains("[table: Table 1: Per-problem resource accounting"));
        assert!(d.md.contains("[fig: Figure 2: Competition score over time (log scale)."));
        assert!(d.md.contains("\n- An empirical study of model post-training"));
        assert!(d.md.contains("\n**Official result.** The system participated officially in IMO"));
        assert!(d.md.contains("P6 continued^† |"));
        assert_eq!(sec_ids(&d)[..3], ["s1", "1", "5"]);
        assert_eq!(d.refs.len(), 25);
        assert!(d.refs[0].text.starts_with("Luoxin Chen, Jinming Gu"));
        // no navigation / footer text
        assert!(!d.md.contains("Instructions for reporting errors"));
        assert!(!d.md.contains("Report GitHub Issue"));
    }

    #[test]
    fn svg_foreignobject_boxes_keep_text() {
        let d = parse(&fx("html_svg_boxes.html"));
        assert!(d.md.contains("Find all functions $f:\\mathbb{R^{+}}\\to\\mathbb{R^{+}}$ such that:"));
    }

    // ----------------------------------------------------------------------- institutions

    #[test]
    fn author_blocks() {
        let thanks = fx("html_authors_thanks.html");
        assert_eq!(
            insts_only(&thanks),
            [
                "Department of Mathematics, Technical University of Munich, and the Munich Center for Machine Learning",
                "School of Mathematics, Institute for Advanced Study, 1 Einstein Drive, Princeton, NJ 08540, USA",
                "Department of Mathematics, University of South Carolina, Columbia. 1523 Greene St, Columbia, SC 29208, USA",
                "Institute of Mathematics, Technische Universität Berlin, Straße des 17. Juni 136, 10623 Berlin, Germany",
            ]
        );

        // address contacts start with the author's own name
        assert_eq!(
            insts_only(&fx("html_authors_address.html")),
            [
                "Department of Mathematical Sciences, Durham University, DH1 3LE, United Kingdom",
                "School of Mathematics and Statistics, University of Sheffield, Sheffield, S3 7RH, United Kingdom"
            ]
        );
        // <br> splits a contact into several institutions
        assert_eq!(
            insts_only(&fx("html_authors_multiline.html")),
            [
                "Department of Applied Mathematics and Theoretical Physics",
                "University of Cambridge, CB3 0WA, UK",
                "CERN, Theoretical Physics Department, Geneva, Switzerland"
            ]
        );
        // dedupe, entities decoded
        let many = insts_only(&fx("html_authors_many.html"));
        assert_eq!(many.len(), 9);
        assert_eq!(many[0], "BCG X AI Science Institute");
        assert_eq!(many[1], "Alice & Bob");
        // `{a, b}@dom` e-mail groups are dropped
        assert_eq!(insts_only(&fx("html_authors_braces.html")), ["University of Southern California"]);
    }

    #[test]
    fn insts_only_scans_only_the_head() {
        let page = fx("html_authors_address.html");
        let pad = "<script>var x = 1;</script>".repeat(10_000);
        let padded = page.replacen("<body>", &format!("<body>{pad}"), 1);
        assert!(padded.find("ltx_authors").unwrap() > 100_000);
        assert!(insts_only(&padded).is_empty(), "authors beyond the 256KB window are not scanned");
        let padded = page.replacen("<body>", &format!("<body>{}", &pad[..70_000]), 1);
        assert!(padded.find("ltx_authors").unwrap() > 64 * 1024);
        assert_eq!(insts_only(&padded).len(), 2);
        assert!(insts_only("").is_empty());
        assert!(insts_only("<div class=\"ltx_authors\"").is_empty());
    }

    #[test]
    fn institutions_synthetic() {
        let body = r#"<div class="ltx_authors"><span class="ltx_creator ltx_role_author"><span class="ltx_personname">Ann Lee</span>
<span class="ltx_author_notes"><span class="ltx_contact ltx_role_affiliation"><span class="ltx_contact_name">Affiliation: </span>MIT
<br class="ltx_break">Dept. of Math, Cambridge MA</span>
<span class="ltx_contact ltx_role_email"><span class="ltx_contact_name">Email: </span><a href="mailto:a@b.c">a@b.c</a></span>
<span class="ltx_note ltx_note_frontmatter ltx_role_thanks"><sup class="ltx_note_mark">†</sup><span class="ltx_note_outer"><span class="ltx_note_content"><span class="ltx_note_type">thanks: </span>University of Oslo, Norway. Supported by NSF grant 123. E-mail: ann@uio.no</span></span></span>
</span></span>
<span class="ltx_creator ltx_role_author"><span class="ltx_personname">Bo Li</span><span class="ltx_contact ltx_role_affiliation">MIT</span></span></div><p>Body</p>"#;
        let d = doc(body);
        assert_eq!(insts_only(body), ["MIT", "Dept. of Math, Cambridge MA", "University of Oslo, Norway"]);
        assert!(!d.md.contains("Ann Lee"));
        assert_eq!(d.md, "Body");
    }

    // ----------------------------------------------------------------------- abs page

    #[test]
    fn abs_pages() {
        let m = abs_meta(&fx("html_abs_msc.html"));
        assert_eq!(m.msc, ["20F55", "14M15", "15B48"]);
        assert_eq!(
            m.versions,
            [
                ("v1".to_string(), "2024-01-29".to_string(), "21kb".to_string()),
                ("v2".to_string(), "2024-02-20".to_string(), "21kb".to_string())
            ]
        );
        let m = abs_meta(&fx("html_abs_versions.html"));
        assert_eq!(m.versions.len(), 7);
        assert_eq!(m.versions[0], ("v1".into(), "2017-06-12".into(), "1102kb".into()));
        assert_eq!(m.versions[5], ("v6".into(), "2023-07-24".into(), "1124kb".into()));
        assert_eq!(m.versions[6], ("v7".into(), "2023-08-02".into(), "1124kb".into()));
        assert!(m.msc.is_empty());
        let m = abs_meta(&fx("html_abs_acm.html"));
        assert_eq!(m.versions, [("v1".to_string(), "2024-01-12".to_string(), "52kb".to_string())]);
    }

    #[test]
    fn abs_synthetic() {
        let h = r#"<td class="tablecell msc-classes">05C10; 05C62 (Primary), 57M15</td>
<td class="tablecell acm-classes">F.2.2, G.2.1</td>
<div class="submission-history"><h2>Submission history</h2> From: A [<a>view email</a>]<br/>
<strong>[v1]</strong> Mon, 5 Feb 2024 10:00:00 UTC (123 KB)<br/>
<strong>[v2]</strong> Tue, 14 May 2024 10:00:00 UTC (12kb, 3 figures)<br/>
<strong>[v10]</strong> Sat, 31 Dec 2033 10:00:00 UTC (1,234 KB)<br/>
</div><div>[v99] Mon, 1 Jan 2000 (1 KB)</div>"#;
        let m = abs_meta(h);
        assert_eq!(m.msc, ["05C10", "05C62 (Primary)", "57M15"]);
        assert_eq!(
            m.versions,
            [
                ("v1".to_string(), "2024-02-05".to_string(), "123kb".to_string()),
                ("v2".to_string(), "2024-05-14".to_string(), "12kb".to_string()),
                ("v10".to_string(), "2033-12-31".to_string(), "1234kb".to_string()),
            ]
        );
        let e = abs_meta("");
        assert!(e.msc.is_empty() && e.versions.is_empty());
        let e = abs_meta("<td class=\"msc-classes\">05C");
        assert_eq!(e.msc, ["05C"]);
    }

    // ----------------------------------------------------------------------- synthetic structure

    #[test]
    fn empty_and_junk_inputs_do_not_panic() {
        for h in [
            "",
            "<",
            "<<<<",
            "</",
            "</div>",
            "<div",
            "<div class=\"ltx_theorem",
            "<math alttext=\"x",
            "<!--",
            "<!-- never closed <div>",
            "<script>never closed",
            "<style>",
            "&",
            "&#x;&#99999999999;&unknown;",
            "<h2 class=\"ltx_title ltx_title_section\">",
            "é<é>é</é>é",
            "<div class='a>x</div>",
            "<table><tr><td>a<td>b<tr><td>c</table>",
            "</span></span></p></li></ul></table>",
            "<math><mi>x</mi>",
        ] {
            let d = parse(h);
            let _ = (d.md, d.title, insts_only(h), abs_meta(h));
            let _ = grep(h, &[], "a", 3, 3);
        }
        let d = parse("");
        assert_eq!((d.md.as_str(), d.title.as_str(), d.sections.len(), d.items.len(), d.refs.len()), ("", "", 0, 0, 0));
    }

    #[test]
    fn huge_attribute_junk() {
        let junk = "x".repeat(300_000);
        let h = format!(
            "<article><div data-a=\"{junk}\" class=\"ltx_para\" style='{junk}'><p>Hello <span title=\"{junk}\">world</span></p></div><a href={junk}>z</a></article>"
        );
        let d = parse(&h);
        assert_eq!(d.md, "Hello world\n\nz");
        let h2 = format!("<article><p>a</p><div {junk}>b</div><p {junk}=\"{junk}\">c</p></article>");
        assert_eq!(parse(&h2).md, "a\n\nb\n\nc");
    }

    #[test]
    fn pathological_nesting_is_linear_and_safe() {
        let n = 100_000;
        let deep = format!("<article>{}x{}", "<div>".repeat(n), "</p>".repeat(n));
        let t = std::time::Instant::now();
        let d = parse(&deep);
        assert_eq!(d.md, "x");
        let spans = format!("<article><p>{}", "<span class=\"ltx_text\">a".repeat(n));
        assert_eq!(parse(&spans).md.len(), n);
        assert!(t.elapsed().as_secs() < 10);
    }

    #[test]
    fn unclosed_tags_recover() {
        let d = doc("<div class=\"ltx_para\"><p>one<p>two<ul><li>a<li>b</ul><p>three <span>x<span>y");
        assert_eq!(d.md, "one\n\ntwo\n\n- a\n- b\n\nthree xy");
        let d = doc("<table class=\"ltx_tabular\"><tr><td>a<td>b<tr><td>c<td>d</table><p>after");
        assert_eq!(d.md, "a | b\nc | d\n\nafter");
        // stray closers are ignored
        let d = doc("</div></span><p>x</p></li>y");
        assert_eq!(d.md, "x\n\ny");
    }

    #[test]
    fn nested_theorem_and_item_text() {
        let d = doc(
            r#"<section class="ltx_section"><h2 class="ltx_title ltx_title_section"><span class="ltx_tag ltx_tag_section">3 </span>Main</h2>
<div class="ltx_theorem ltx_theorem_theorem"><h6 class="ltx_title ltx_runin ltx_title_theorem"><span class="ltx_tag ltx_tag_theorem"><span class="ltx_text ltx_font_bold">Theorem 3.1</span></span><span class="ltx_text ltx_font_bold">.</span></h6>
<div class="ltx_para"><p class="ltx_p">Outer <math alttext="x^2" display="inline"><mi>x</mi></math>.</p></div>
<div class="ltx_theorem ltx_theorem_remark"><h6 class="ltx_title ltx_runin ltx_title_theorem"><span class="ltx_tag ltx_tag_theorem">Remark 3.2</span>.</h6>
<div class="ltx_para"><p class="ltx_p">Inner.</p></div></div>
<div class="ltx_para"><p class="ltx_p">Tail.</p></div></div>
<div class="ltx_proof"><h6 class="ltx_title ltx_runin ltx_title_proof">Proof.</h6><div class="ltx_para"><p class="ltx_p">By magic.</p></div></div>
<div class="ltx_theorem ltx_theorem_proof"><h6 class="ltx_title ltx_runin ltx_title_theorem"><span class="ltx_tag ltx_tag_theorem">Proof 3.3</span>.</h6><p class="ltx_p">skipped</p></div>
<div class="ltx_theorem ltx_theorem_thmx"><p class="ltx_p">no title at all</p></div>
</section>"#,
        );
        assert_eq!(labels(&d), ["Theorem 3.1", "Remark 3.2", "Item"]);
        assert_eq!(d.items[0].kind, Kind::Theorem);
        assert_eq!(d.items[1].kind, Kind::Remark);
        assert_eq!(d.items[2].kind, Kind::Other);
        assert_eq!(d.items[0].text, "Outer $x^2$.\n\n**Remark 3.2.** Inner.\n\nTail.");
        assert_eq!(d.items[1].text, "Inner.");
        assert_eq!(d.items[2].text, "no title at all");
        assert!(d.items.iter().all(|i| i.sec == "3"));
        assert_eq!(
            d.md,
            "## 3 Main\n\n**Theorem 3.1.** Outer $x^2$.\n\n**Remark 3.2.** Inner.\n\nTail.\n\n**Proof.** By magic.\n\n**Proof 3.3.** skipped\n\nno title at all"
        );
    }

    #[test]
    fn math_in_titles_and_display_math() {
        let d = doc(
            r#"<section class="ltx_section"><h2 class="ltx_title ltx_title_section"><span class="ltx_tag ltx_tag_section">1 </span>On <math alttext="\mathbb{F}_{q}" display="inline"><mi>F</mi></math>-codes<span class="ltx_note ltx_role_footnote"><sup class="ltx_note_mark">1</sup><span class="ltx_note_outer"><span class="ltx_note_content">dropped</span></span></span></h2>
<p class="ltx_p">Let <math alttext="a &lt; b" display="inline"></math> and
<math alttext="\displaystyle\sum_{i}
   x_i" display="block"><mi>x</mi></math> done<math display="inline"><semantics><annotation encoding="application/x-tex">y_1</annotation></semantics></math>.</p></section>"#,
        );
        assert_eq!(d.sections[0].title, "On $\\mathbb{F}_{q}$-codes");
        assert_eq!(d.sections[0].id, "1");
        assert_eq!(d.md, "## 1 On $\\mathbb{F}_{q}$-codes\n\nLet $a < b$ and\n\n$$\\sum_{i} x_i$$\n\ndone$y_1$.");
    }

    #[test]
    fn lists_tables_figures_listings() {
        let d = doc(
            r#"<ul class="ltx_itemize"><li class="ltx_item"><span class="ltx_tag ltx_tag_item">•</span><div class="ltx_para"><p class="ltx_p">first</p></div></li>
<li class="ltx_item"><span class="ltx_tag ltx_tag_item">•</span><div class="ltx_para"><p class="ltx_p">second<ul><li class="ltx_item"><span class="ltx_tag ltx_tag_item">•</span> nested</li></ul></p></div></li></ul>
<ol class="ltx_enumerate"><li class="ltx_item"><span class="ltx_tag ltx_tag_item">(a)</span><div class="ltx_para"><p class="ltx_p">alpha</p></div></li></ol>
<table class="ltx_tabular"><thead><tr><th>h1</th><th>h2</th></tr></thead><tbody><tr><td>a</td><td></td><td>c</td></tr><tr><td><span class="ltx_rule"></span></td></tr><tr><td></td><td>z</td></tr></tbody></table>
<figure class="ltx_figure"><img src="x.png" alt="pic"><figcaption class="ltx_caption"><span class="ltx_tag ltx_tag_figure">Figure 1: </span>A <em>nice</em> plot.</figcaption></figure>
<figure class="ltx_figure"><img src="y.png"></figure>
<div class="ltx_listing ltx_lstlisting"><div class="ltx_listing_data"><a href="data:x">dl</a></div><div class="ltx_listingline"><span class="ltx_text"> x</span><span class="ltx_text ltx_lst_space">  </span><span>=1</span>
</div>
<div class="ltx_listingline">
</div><div class="ltx_listingline">y</div></div>
<pre>a  b
c</pre>
<p class="ltx_p">end</p>"#,
        );
        assert_eq!(
            d.md,
            "- first\n- second\n  - nested\n\n- (a) alpha\n\nh1 | h2\na |  | c\n| z\n\n[fig: Figure 1: A nice plot.]\n\n x  =1\n\ny\n\na  b\nc\n\nend"
        );
    }

    #[test]
    fn skipped_regions_and_entities() {
        let d = parse(
            "<html><head><title>T &amp; U</title><style>p{}</style><script>if (a<b) {document.write('<p>x</p>')}</script></head>\
<body><nav>menu</nav><p>a&nbsp;b &lt;c&gt; &#233; &#x41; &bogus; &amp</p><footer>foot</footer><SCRIPT>1</SCRIPT><button>no</button><p>z</p></body></html>",
        );
        assert_eq!(d.md, "a b <c> é A &bogus; &amp\n\nz");
        assert_eq!(d.title, "T & U");
        let d = parse("<title>Untitled Document</title><p>x</p>");
        assert_eq!(d.title, "");
        assert_eq!(d.md, "x");
    }

    #[test]
    fn sup_sub_and_br() {
        let d = doc("<p>x<sup>2</sup> y<sub>ij</sub> z<sup></sup> w<sup class=\"ltx_note_mark\">9</sup><br>next</p>");
        assert_eq!(d.md, "x^2 y_(ij) z w\nnext");
    }

    #[test]
    fn unnumbered_sections_get_sequential_ids() {
        let d = doc(
            r#"<h2 class="ltx_title ltx_title_section">Intro</h2><p>a</p><h3 class="ltx_title ltx_title_subsection">Deep</h3><p>b</p>
<h2 class="ltx_title ltx_title_section"><span class="ltx_tag ltx_tag_section">2 </span>Num</h2><p>c</p>
<h2 class="ltx_title ltx_title_appendix"><span class="ltx_tag ltx_tag_appendix">Appendix A </span>Extra</h2><h3 class="ltx_title ltx_title_subsection"><span class="ltx_tag ltx_tag_subsection">A.1 </span>Sub</h3>"#,
        );
        assert_eq!(sec_ids(&d), ["s1", "s2", "2", "A", "A.1"]);
        assert_eq!(d.sections[3].title, "Extra");
        assert_eq!(d.sections[0].end, d.sections[2].start - 2);
        assert_eq!(d.sections[1].start, d.sections[0].start + "## Intro\n\na\n\n".len());
        assert_eq!(d.md, "## Intro\n\na\n\n### Deep\n\nb\n\n## 2 Num\n\nc\n\n## Appendix A Extra\n\n### A.1 Sub");
    }

    #[test]
    fn refs_parsing() {
        let d = doc(
            r#"<ul class="ltx_biblist"><li class="ltx_bibitem"><span class="ltx_tag ltx_tag_bibitem">[7]</span><span class="ltx_bibblock">A. Author. Title. arXiv:2301.01234v2 [math.CO], 2023.</span></li>
<li class="ltx_bibitem"><span class="ltx_tag ltx_role_refnum ltx_tag_bibitem">Smith (2020)</span><span class="ltx_bibblock">Smith. Old. <a href="https://arxiv.org/abs/math/0309285">link</a></span><span class="ltx_bibblock">doi:10.1090/S0002-9904-1950-09475-0.</span></li>
<li class="ltx_bibitem"><span class="ltx_tag ltx_tag_bibitem">[9]</span><span class="ltx_bibblock">CoRR, abs/1409.0473, 2014. <a href="https://doi.org/10.48550/arXiv.2201.00001">x</a></span></li>
<li class="ltx_bibitem"><span class="ltx_bibblock">no tag arXiv preprint arXiv:hep-th/9901001</span></li></ul>"#,
        );
        assert_eq!(d.refs.len(), 4);
        assert_eq!((d.refs[0].n, d.refs[0].arxiv.as_deref(), d.refs[0].doi.as_deref()), (7, Some("2301.01234"), None));
        assert_eq!(d.refs[0].text, "A. Author. Title. arXiv:2301.01234v2 [math.CO], 2023.");
        assert_eq!((d.refs[1].n, d.refs[1].arxiv.as_deref(), d.refs[1].doi.as_deref()), (2, Some("math/0309285"), Some("10.1090/S0002-9904-1950-09475-0")));
        assert_eq!(d.refs[1].text, "Smith. Old. link doi:10.1090/S0002-9904-1950-09475-0.");
        assert_eq!((d.refs[2].n, d.refs[2].arxiv.as_deref(), d.refs[2].doi.as_deref()), (9, Some("1409.0473"), None));
        assert_eq!(d.refs[3].arxiv.as_deref(), Some("hep-th/9901001"));
        assert_eq!(d.refs[3].n, 4);
    }

    // ----------------------------------------------------------------------- find_section

    fn secs() -> Vec<Section> {
        let mk = |id: &str, title: &str| Section { id: id.into(), level: 1, title: title.into(), start: 0, end: 0 };
        vec![mk("1", "Introduction"), mk("2", "Main Results"), mk("2.1", "Main theorem"), mk("A", "Appendix: Proofs"), mk("s1", "Results")]
    }

    #[test]
    fn find_section_order() {
        let s = secs();
        assert_eq!(find_section(&s, "2.1").unwrap().title, "Main theorem");
        assert_eq!(find_section(&s, " 2 ").unwrap().id, "2");
        assert_eq!(find_section(&s, "§2.1").unwrap().id, "2.1");
        assert_eq!(find_section(&s, "a").unwrap().id, "A");
        assert_eq!(find_section(&s, "RESULTS").unwrap().id, "s1");
        assert_eq!(find_section(&s, "main").unwrap().id, "2");
        assert_eq!(find_section(&s, "proofs").unwrap().id, "A");
        assert_eq!(find_section(&s, "intro").unwrap().id, "1");
        assert!(find_section(&s, "nope").is_none());
        assert!(find_section(&s, "").is_none());
        assert!(find_section(&[], "1").is_none());
    }

    // ----------------------------------------------------------------------- grep

    fn gsec(id: &str, start: usize, end: usize, level: u8) -> Section {
        Section { id: id.into(), level, title: id.into(), start, end }
    }

    #[test]
    fn grep_literal_basics() {
        let text = "Alpha beta\nGAMMA alpha ALPHA\n\nalphabet";
        let secs = [gsec("1", 0, 28, 1), gsec("1.1", 11, 28, 2), gsec("2", 30, 39, 1)];
        let h = grep(text, &secs, "alpha", 3, 10);
        let got: Vec<(usize, &str, &str)> = h.iter().map(|h| (h.pos, h.sec.as_str(), h.snippet.as_str())).collect();
        assert_eq!(
            got,
            [
                (0, "1", "Alpha be…"),
                (17, "1.1", "…MA alpha AL…"),
                (23, "1.1", "…ha ALPHA  a…"),
                (30, "2", "…A  alphabet"),
            ]
        );
        let h = grep(text, &secs, "ALPHA", 0, 2);
        assert_eq!(h.len(), 2);
        assert_eq!(h[1].snippet, "…alpha…");
        assert!(grep(text, &secs, "zzz", 3, 10).is_empty());
        assert!(grep(text, &secs, "", 3, 10).is_empty());
        assert!(grep(text, &secs, "alpha", 3, 0).is_empty());
        // non-overlapping
        let h = grep("aaaaa", &[], "aa", 0, 10);
        assert_eq!(h.iter().map(|h| h.pos).collect::<Vec<_>>(), [0, 2]);
        // pattern longer than the text
        assert!(grep("ab", &[], "abc", 1, 5).is_empty());
    }

    #[test]
    fn grep_unicode_is_boundary_safe() {
        let text = "ééé Ünïcödé straße ΣΑΣ 数学 中文 end";
        let h = grep(text, &[], "ünï", 2, 5);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].pos, text.find("Ünï").unwrap());
        assert_eq!(h[0].snippet, "…é Ünïcö…");
        let h = grep(text, &[], "数学", 1, 5);
        assert_eq!(h[0].snippet, "… 数学 …");
        let h = grep(text, &[], "σασ", 0, 5);
        assert_eq!(h.len(), 1);
        // ASCII needle never matches inside multi-byte sequences; snippets never split chars
        for ctx in 0..6 {
            for pat in ["e", "é", "n", "中", "end", " "] {
                for hit in grep(text, &[], pat, ctx, 100) {
                    assert!(text.is_char_boundary(hit.pos));
                    assert!(!hit.snippet.contains('\u{fffd}'));
                }
            }
        }
        let h = grep("日本語日本語", &[], "本", 1, 10);
        assert_eq!(h.iter().map(|h| h.pos).collect::<Vec<_>>(), [3, 12]);
    }

    #[test]
    fn grep_snippet_edges() {
        let text = "0123456789\nabc\r\ndef";
        let h = grep(text, &[], "abc", 4, 1);
        assert_eq!(h[0].snippet, "…789 abc  de…");
        let h = grep(text, &[], "0123", 100, 1);
        assert_eq!(h[0].snippet, "0123456789 abc  def");
        let h = grep(text, &[], "def", 2, 1);
        assert_eq!(h[0].snippet, "…\r\ndef".replace('\r', " ").replace('\n', " "));
        // huge regex match is clipped
        let long = "x".repeat(10_000);
        let h = grep(&long, &[], "/x+/", 5, 1);
        assert!(h[0].snippet.len() < 200);
        assert!(h[0].snippet.ends_with('…'));
    }

    fn rx(text: &str, pat: &str) -> Vec<(usize, usize)> {
        // (pos, matched length) via a ctx-0 snippet is lossy, so compare against pos and the text itself
        grep(text, &[], pat, 0, 100).iter().map(|h| (h.pos, h.snippet.trim_matches('…').chars().count())).collect()
    }

    #[test]
    fn grep_regex() {
        assert_eq!(rx("a1 b22 c333", "/\\d+/"), [(1, 1), (4, 2), (8, 3)]);
        assert_eq!(rx("foo bar foobar", "/foo|bar/"), [(0, 3), (4, 3), (8, 3), (11, 3)]);
        assert_eq!(rx("color colour colr", "/colou?r/"), [(0, 5), (6, 6)]);
        assert_eq!(rx("abcabc", "/(abc)+/"), [(0, 6)]);
        assert_eq!(rx("ab12 cd", "/[a-c]+/"), [(0, 2), (5, 1)]);
        assert_eq!(rx("ab12 cd", "/[^a-z ]+/"), [(2, 2)]);
        assert_eq!(rx("one two  three", "/\\w+\\s+\\w+/"), [(0, 7)]);
        assert_eq!(rx("cat concat cat", "/\\bcat\\b/"), [(0, 3), (11, 3)]);
        assert_eq!(rx("cat concat", "/\\Bcat/"), [(7, 3)]);
        assert_eq!(rx("a.b axb", "/a\\.b/"), [(0, 3)]);
        assert_eq!(rx("a.b axb", "/a.b/"), [(0, 3), (4, 3)]);
        assert_eq!(rx("line1\nline2\nxline3", "/^line\\d$/"), [(0, 5), (6, 5)]);
        assert_eq!(rx("Problem 12. Problem 3:", "/Problem \\d+[.:]/"), [(0, 11), (12, 10)]);
        assert_eq!(rx("aaa", "/a{2}/"), [(0, 2)]);
        assert_eq!(rx("aaaa", "/a{2,3}/"), [(0, 3)]);
        assert_eq!(rx("xaay", "/a{1,}/"), [(1, 2)]);
        assert_eq!(rx("<a><b>", "/<.+?>/"), [(0, 3), (3, 3)]);
        assert_eq!(rx("<a><b>", "/<.+>/"), [(0, 6)]);
        assert_eq!(rx("ABC abc", "/abc/"), [(4, 3)]);
        assert_eq!(rx("ABC abc", "/abc/i"), [(0, 3), (4, 3)]);
        assert_eq!(rx("ABC", "/[a-b]+/i"), [(0, 2)]);
        assert_eq!(rx("é1 ü2", "/\\w\\d/"), [(0, 2), (4, 2)]);
        assert_eq!(rx("aXb", "/a(?:X|Y)b/"), [(0, 3)]);
        // empty matches are not hits; the loop terminates
        assert_eq!(rx("abc", "/x*/"), []);
        assert_eq!(rx("", "/a*/"), []);
    }

    #[test]
    fn grep_bad_regex_falls_back_to_literal() {
        // not a valid regex: searched literally (case-insensitively), never panics
        let h = grep("a (b c) d", &[], "/(b/", 0, 5);
        assert!(h.is_empty());
        let h = grep("x /(b/ y", &[], "/(b/", 0, 5);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].pos, 2);
        for p in ["/*/", "/+a/", "/[a/", "/a)/", "/(/", "/\\/", "//", "/a{2,1}/", "/a**/", "/[z-a]/", "/(((((((((((((((((a)))))))))))))))))/"] {
            let _ = grep("aaa(b [z a", &[], p, 1, 5);
        }
        // single slash and mid-pattern slashes are literals
        assert_eq!(grep("a/b", &[], "a/b", 0, 5).len(), 1);
        assert_eq!(grep("/x", &[], "/", 0, 5).len(), 1);
    }

    #[test]
    fn grep_regex_catastrophic_pattern_terminates() {
        let text = format!("{}!", "a".repeat(40));
        let t = std::time::Instant::now();
        let h = grep(&text, &[], "/(a+)+b/", 2, 5);
        // no real match exists; a budget-exhaustion notice (never a bogus hit) is acceptable
        assert!(h.iter().all(|x| x.snippet.contains("too complex")), "{h:?}");
        assert!(t.elapsed().as_secs() < 5, "step budget must bound backtracking");
    }

    #[test]
    fn grep_deep_repetition_does_not_overflow_stack() {
        let text = format!("{}z", "a".repeat(200_000));
        let h = grep(&text, &[], "/a*z/", 1, 5);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].pos, 0);
        let h = grep(&text, &[], "/[a-z]+/", 1, 5);
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn grep_sections_innermost_and_gaps() {
        let secs = [gsec("1", 10, 100, 1), gsec("1.1", 20, 40, 2), gsec("1.2", 40, 90, 2), gsec("2", 100, 200, 1)];
        assert_eq!(sec_at(&secs, 5), "");
        assert_eq!(sec_at(&secs, 10), "1");
        assert_eq!(sec_at(&secs, 25), "1.1");
        assert_eq!(sec_at(&secs, 40), "1.2");
        assert_eq!(sec_at(&secs, 95), "1");
        assert_eq!(sec_at(&secs, 100), "2");
        assert_eq!(sec_at(&secs, 250), "");
        assert_eq!(sec_at(&[], 3), "");
    }

    #[test]
    fn grep_on_parsed_fixture() {
        let d = parse(&fx("html_math_anticodes.html"));
        let h = grep(&d.md, &d.sections, "open problem", 20, 10);
        assert_eq!(h.len(), 2, "{:?}", h.iter().map(|h| &h.snippet).collect::<Vec<_>>());
        assert_eq!(h[0].sec, "5");
        assert!(h[0].snippet.contains("open problems") || h[0].snippet.to_lowercase().contains("open problem"));
        let h = grep(&d.md, &d.sections, "/Problem 5\\.\\d/", 5, 10);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].sec, "5.2");
        assert_eq!(h[1].sec, "5.3");
        assert!(d.md[h[0].pos..].starts_with("Problem 5.1"));
        let h = grep(&d.md, &d.sections, "hamming", 10, 3);
        assert_eq!(h.len(), 3);
        let h = grep(&d.md, &d.sections, "ahlswede", 10, 100);
        assert!(h.iter().any(|h| h.sec == "s2"));
    }

    // ----------------------------------------------------------------------- performance

    #[test]
    #[ignore]
    fn bench_parse() {
        let page = match std::env::var("ARX_BENCH_HTML") {
            Ok(p) => std::fs::read_to_string(p).unwrap(),
            Err(_) => {
                // ~500KB: the real anticodes fixture body repeated
                let base = fx("html_math_anticodes.html");
                let a = base.find("<article").unwrap();
                let b = base.find("</article>").unwrap();
                format!("{}{}{}", &base[..a], base[a..b].repeat(8), &base[b..])
            }
        };
        let mut best = std::time::Duration::MAX;
        for _ in 0..50 {
            let t = std::time::Instant::now();
            let d = parse(&page);
            std::hint::black_box(&d);
            best = best.min(t.elapsed());
        }
        let t = std::time::Instant::now();
        let d = parse(&page);
        let h = grep(&d.md, &d.sections, "hamming", 40, 1000);
        std::hint::black_box(&h);
        eprintln!("parse {}KB: best {:?}; parse+grep once {:?}; md {}KB", page.len() / 1024, best, t.elapsed(), d.md.len() / 1024);
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;

    #[test]
    fn budget_exhaustion_is_reported_even_after_earlier_hits() {
        let text = format!("ok {}!", "a".repeat(40));
        let h = grep(&text, &[], "/ok|(a+)+b|a{40}!/", 0, 5);
        assert!(h.iter().any(|x| x.snippet.contains("too complex")), "{h:?}");
    }

    #[test]
    fn html_links_carry_the_arxiv_id() {
        assert_eq!(make_ref(1, "", "Paper".into(), "https://arxiv.org/html/2401.06512v1").arxiv.as_deref(), Some("2401.06512"));
        assert_eq!(make_ref(1, "", "Paper".into(), "https://arxiv.org/pdf/2401.06512").arxiv.as_deref(), Some("2401.06512"));
    }

    #[test]
    fn arxiv_doi_prefix_is_case_insensitive() {
        let r = make_ref(1, "", "x".into(), "https://doi.org/10.48550/ARXIV.1706.03762");
        assert_eq!((r.arxiv.as_deref(), r.doi.as_deref()), (Some("1706.03762"), None));
    }

    #[test]
    fn doi_keeps_balanced_parentheses() {
        assert_eq!(find_doi("doi:10.1061/(ASCE)0733-9410(1990)116:8(1231)").as_deref(), Some("10.1061/(ASCE)0733-9410(1990)116:8(1231)"));
        assert_eq!(find_doi("(see doi:10.1000/abc123).").as_deref(), Some("10.1000/abc123"));
    }

    #[test]
    fn doi_with_angle_brackets_is_kept_whole() {
        assert_eq!(
            find_doi("Stat. Med. 16 (1997) 385. doi:10.1002/(sici)1097-0258(19970228)16:4<385::aid-sim380>3.0.co;2-3"),
            Some("10.1002/(sici)1097-0258(19970228)16:4<385::aid-sim380>3.0.co;2-3".to_string())
        );
    }

    #[test]
    fn grouped_regex_repetition_does_not_overflow_stack() {
        // matcher recursion is deep: run on a big-stack thread like the CLI runtime does
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(|| {
                let text = "a".repeat(50_000);
                let _ = grep(&text, &[], "/(a)+/", 10, 3);
                assert_eq!(grep(&"a".repeat(500), &[], "/^(a|b)+$/", 0, 1).len(), 1);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn abs_meta_unterminated_cell_with_multibyte_tail() {
        let page = format!("<td class=\"msc-classes\">{}", "€".repeat(1000));
        let _ = abs_meta(&page);
    }
}

#[cfg(test)]
mod fast_paths {
    use super::*;

    fn pages() -> Vec<(String, String)> {
        let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let mut v: Vec<(String, String)> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().into_string().unwrap();
                (n.starts_with("html_") && !n.starts_with("html_abs_")).then(|| (n.clone(), std::fs::read_to_string(format!("{dir}/{n}")).unwrap()))
            })
            .collect();
        v.sort();
        v.push((
            "list+theorem".into(),
            concat!(
                "<article><h1 class=\"ltx_title ltx_title_document\">T &amp; U</h1>",
                "<h2 class=\"ltx_title ltx_title_section\"><span class=\"ltx_tag ltx_tag_section\">1 </span>Intro</h2>",
                "<ul class=\"ltx_itemize\"><li class=\"ltx_item\"><span class=\"ltx_tag ltx_tag_item\">\u{2022}</span>",
                "<div class=\"ltx_para\"><p class=\"ltx_p\">before <math alttext=\"x\"><mi>x</mi></math> text</p></div>",
                "<div class=\"ltx_theorem ltx_theorem_problem\"><h6 class=\"ltx_title ltx_runin ltx_title_theorem\">Problem 1.</h6>",
                "<p>Is <math alttext=\"\\displaystyle a &lt; b\" display=\"block\"><mi>a</mi></math>\n true?&nbsp;</p></div></li></ul>",
                "<span class=\"ltx_note ltx_role_footnote\">note <b>here</b></span></article>"
            )
            .into(),
        ));
        v.push((
            "table+bib".into(),
            concat!(
                "<article><section><h2 class=\"ltx_title ltx_title_section\">Data</h2><table class=\"ltx_tabular\"><tr><td>a  b</td><td>",
                "<div class=\"ltx_theorem ltx_theorem_lemma\"><h6 class=\"ltx_title ltx_runin ltx_title_theorem\">Lemma 2.</h6>cell</div></td></tr></table></section>",
                "<ul class=\"ltx_biblist\"><li class=\"ltx_bibitem\"><span class=\"ltx_tag ltx_tag_bibitem\">[3]</span>",
                "<span class=\"ltx_bibblock\">A. B. <a href=\"https://arxiv.org/abs/1706.03762\">Attention</a>, 2017.</span></li></ul></article>"
            )
            .into(),
        ));
        v.push((
            "pipes_in_cells".into(),
            "<article><table><tr><td>|<div class=\"ltx_theorem ltx_theorem_problem\">|||||</div></td></tr></table><h2>\u{1F600}abcdef</h2></article>".into(),
        ));
        v.push((
            "bullet_tag_after_theorem".into(),
            "<article><table><tr><td>\n<div class=\"ltx_theorem ltx_theorem_problem\">foo|</div>\n<ul><li><span class=\"ltx_tag ltx_tag_item\">\u{2022}</span></li></ul>\n</td></tr></table></article>".into(),
        ));
        v.push((
            "entity_pipe_after_theorem".into(),
            "<article><table><tr><td><div class=\"ltx_theorem ltx_theorem_problem\">foo&#124;</div></td><td>&#124;</td></tr></table></article>".into(),
        ));
        v.push((
            "pipe_after_theorem".into(),
            "<article><table><tr><td><div class=\"ltx_theorem ltx_theorem_problem\">foo|</div></td><td>|</td></tr></table></article>".into(),
        ));
        v
    }

    fn dbg<T: std::fmt::Debug>(t: &T) -> String {
        format!("{t:?}")
    }

    #[test]
    fn want_modes_match_the_full_parse() {
        for (name, h) in pages() {
            let all = parse(&h);
            let text = parse_for(&h, Want::Text);
            assert_eq!((&text.title, &text.md, dbg(&text.sections)), (&all.title, &all.md, dbg(&all.sections)), "{name}: text");
            let outline = parse_for(&h, Want::Outline);
            let key = |d: &Doc| d.sections.iter().map(|s| (s.id.clone(), s.level, s.title.clone())).collect::<Vec<_>>();
            assert_eq!((&outline.title, key(&outline)), (&all.title, key(&all)), "{name}: outline");
            let items = parse_for(&h, Want::Items);
            assert_eq!((&items.title, dbg(&items.items)), (&all.title, dbg(&all.items)), "{name}: items");
            let refs = parse_for(&h, Want::Refs);
            assert_eq!((&refs.title, dbg(&refs.refs)), (&all.title, dbg(&all.refs)), "{name}: refs");
        }
    }

    #[test]
    fn want_modes_match_the_full_parse_on_random_documents() {
        const TAGS: &[(&str, &str)] = &[
            ("<table>", "</table>"), ("<tr>", "</tr>"), ("<td>", "</td>"), ("<div class=\"ltx_theorem ltx_theorem_problem\">", "</div>"),
            ("<div class=\"ltx_theorem ltx_theorem_lemma\">", "</div>"), ("<ul>", "</ul>"), ("<li>", "</li>"),
            ("<span class=\"ltx_tag ltx_tag_item\">", "</span>"), ("<h2 class=\"ltx_title ltx_title_section\">", "</h2>"),
            ("<h6 class=\"ltx_title ltx_runin ltx_title_theorem\">", "</h6>"), ("<p>", "</p>"), ("<sup>", "</sup>"), ("<sub>", "</sub>"),
            ("<dl>", "</dl>"), ("<dt>", "</dt>"), ("<figure>", "</figure>"), ("<figcaption>", "</figcaption>"), ("<span class=\"ltx_note\">", "</span>"),
            ("<ul class=\"ltx_biblist\">", "</ul>"), ("<li class=\"ltx_bibitem\">", "</li>"), ("<span class=\"ltx_tag ltx_tag_bibitem\">", "</span>"),
            ("<span class=\"ltx_bibblock\">", "</span>"), ("<math><mi>x</mi>", "</math>"),
        ];
        const TEXT: &[&str] = &["|", "||", " | ", "foo", "foo|", "\u{2022}", "-", "&bull;", "&#124;", "&amp;", "x", " ", "\u{1F600}abc", "[1]", "a b  c", "1.", "(", "\n", "é"];
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        thread_local!(static LOC: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) });
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|i| LOC.with(|l| *l.borrow_mut() = i.location().map(|x| x.to_string()).unwrap_or_default())));
        let mut fails: std::collections::BTreeMap<String, (usize, String)> = Default::default();
        let iters: usize = std::env::var("ARX_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
        for _ in 0..iters {
            let mut h = String::from("<article>");
            let mut open: Vec<&str> = Vec::new();
            for _ in 0..rnd(24) + 2 {
                match rnd(3) {
                    0 => {
                        let (o, c) = TAGS[rnd(TAGS.len())];
                        h.push_str(o);
                        open.push(c);
                    }
                    1 => h.push_str(open.pop().unwrap_or("")),
                    _ => h.push_str(TEXT[rnd(TEXT.len())]),
                }
            }
            while let Some(c) = open.pop() {
                h.push_str(c);
            }
            h.push_str("</article>");
            let key = |d: &Doc| d.sections.iter().map(|s| (s.id.clone(), s.level, s.title.clone())).collect::<Vec<_>>();
            let all = std::panic::catch_unwind(|| parse(&h));
            let all = match all {
                Ok(a) => a,
                Err(_) => {
                    *fails.entry(format!("All panics at {}", LOC.with(|l| l.borrow().clone()))).or_insert((0, h.clone())) = (0, h.clone());
                    continue;
                }
            };
            for w in [Want::Items, Want::Refs, Want::Outline, Want::Text] {
                let Ok(got) = std::panic::catch_unwind(|| parse_for(&h, w)) else {
                    let e = fails.entry(format!("{w:?} panics at {}", LOC.with(|l| l.borrow().clone()))).or_insert((0, h.clone()));
                    e.0 += 1;
                    continue;
                };
                let same = match w {
                    Want::Items => dbg(&all.items) == dbg(&got.items),
                    Want::Refs => dbg(&all.refs) == dbg(&got.refs),
                    Want::Outline => key(&all) == key(&got),
                    _ => all.md == got.md,
                };
                if !same {
                    fails.entry(format!("{w:?} differs")).or_insert((0, h.clone())).0 += 1;
                }
            }
        }
        std::panic::set_hook(prev);
        for (k, (n, h)) in &fails {
            println!("{k} x{n}: {h:?}");
        }
        assert!(fails.keys().all(|k| k.starts_with("All panics")), "{fails:#?}"); // `All` panics on a few degenerate pages, as the original parser did
    }

    #[test]
    fn regex_start_filter_keeps_case_folded_and_unicode_matches() {
        let pos = |text: &str, pat: &str| grep(text, &[], pat, 0, 9).iter().map(|h| h.pos).collect::<Vec<_>>();
        assert_eq!(pos("x\u{212A}y", "/k/i"), [1], "KELVIN SIGN folds to k");
        assert_eq!(pos("a\u{130}b", "/i/i"), [1], "dotted capital I lowercases to i");
        assert_eq!(pos("h\u{e9}\u{e9}", "/\u{e9}+/"), [1]);
        assert_eq!(pos("baab", "/a*/"), [1]);
        assert!(pos("abc\n\nd", "/^$/").is_empty());
        assert_eq!(pos("ab\n#c", "/^#/"), [3]);
    }

    #[test]
    fn byte_and_pattern_search_agree_with_the_naive_scan() {
        let s = b"0123456789abcdef</math>tail</MATH></mat</math";
        for from in 0..=s.len() {
            for x in [b'<', b'>', b'0', b'z'] {
                assert_eq!(find_byte(s, from, x), s[from..].iter().position(|&c| c == x).map(|p| p + from), "byte {x} from {from}");
            }
            let naive = |p: &[u8]| s[from..].windows(p.len()).position(|w| w == p).map(|q| q + from);
            assert_eq!(find_pat(s, from, b"</math", &MATH_END_LO), naive(b"</math"), "from {from}");
            assert_eq!(find_pat(s, from, b"</MATH", &MATH_END_UP), naive(b"</MATH"), "from {from}");
        }
    }

    #[test]
    fn heading_levels_follow_part_and_chapter_anywhere_in_the_page() {
        let levels = |html: &str| Parser::new(html, Want::Outline, 0).levels();
        assert_eq!(levels("<x class=ltx_title_section>"), (1, 1));
        assert_eq!(levels("ltx_title_part"), (2, 2));
        assert_eq!(levels("xltx_title_chapter"), (1, 2));
        assert_eq!(levels("ltx_title_ltx_title_chapter ltx_title_part"), (2, 3));
        assert_eq!(levels("ltx_title_par ltx_title_chap ltx_ltx_title_"), (1, 1));
        assert_eq!(levels(""), (1, 1));
    }

    #[test]
    fn clean_tex_drops_displaystyle_and_collapses_words() {
        let clean = |alt: &str| {
            let (mut dec, mut out) = (String::new(), String::new());
            clean_tex(alt, &mut dec, &mut out);
            out
        };
        assert_eq!(clean(" \\displaystyle  x  y "), "x y");
        assert_eq!(clean("a\\displaystyleb c"), "ab c");
        assert_eq!(clean("a \\displaystyle b"), "a b");
        assert_eq!(clean("x &lt; y\n\tz"), "x < y z");
        assert_eq!(clean("\\displaystyle"), "");
        assert_eq!(clean("  "), "");
    }

    #[test]
    fn text_nodes_normalise_ascii_and_unicode_whitespace() {
        let md = |body: &str| parse(&format!("<article><p>{body}</p></article>")).md;
        assert_eq!(md("  a \n\t b   c  "), "a b c");
        assert_eq!(md("a\u{a0}\u{2003}b \u{e9}  c"), "a b \u{e9} c");
        assert_eq!(md("a<b>b</b> <i> c</i>"), "ab c");
        assert_eq!(collapse_ws(" a \n b\u{a0}c "), "a b c");
    }

    #[test]
    fn institution_sentences_split_after_period_and_capital() {
        fn split(s: &str) -> Vec<&str> {
            sentences(s).collect()
        }
        assert_eq!(split("Dept. of Math, Univ. of X. Supported by Y."), ["Dept. of Math, Univ. of X.", "Supported by Y."]);
        assert_eq!(split("Grant 5. Funded by Z"), ["Grant 5. Funded by Z"]);
        assert_eq!(split(". A"), [". A"]);
        assert!(split("  ").is_empty());
    }
}
