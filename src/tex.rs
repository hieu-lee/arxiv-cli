//! LaTeX source handling: e-print unpacking, flattening, outline and theorem-like item extraction.
//!
//! Everything works on a *masked* copy of the source (comments and verbatim bodies blanked with
//! spaces, same length) so byte offsets found in the mask are valid, char-boundary-safe offsets
//! into the original text.
use crate::model::*;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read;

const MAX_TOTAL: usize = 64 << 20;
const MAX_FILE: usize = 16 << 20;
const MAX_DEPTH: usize = 8;
const TITLE_LIM: usize = 4000;

// ---------------------------------------------------------------- unpack

/// Unpack an arxiv.org/e-print payload (gzip'd tar, gzip'd single file, or raw tex) into (name, content) text files.
pub fn unpack(bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    if bytes.is_empty() {
        return Err("empty payload".into());
    }
    let (data, gz_name);
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut dec = flate2::read::GzDecoder::new(bytes);
        // the gzip trailer stores the uncompressed size (mod 2^32): a capacity hint that avoids regrowth
        let hint = bytes.last_chunk::<4>().map_or(0, |t| u32::from_le_bytes(*t) as usize);
        // deflate cannot expand beyond ~1032:1, so a lying trailer cannot force a huge reservation
        let mut buf = Vec::with_capacity(hint.min(MAX_TOTAL).min(bytes.len().saturating_mul(1032)) + 1);
        (&mut dec)
            .take(MAX_TOTAL as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| format!("gzip: {e}"))?;
        if buf.len() > MAX_TOTAL {
            return Err("decompressed size exceeds 64 MB".into());
        }
        gz_name = dec
            .header()
            .and_then(|h| h.filename())
            .map(|f| String::from_utf8_lossy(f).into_owned());
        data = buf;
    } else {
        if bytes.len() > MAX_TOTAL {
            return Err("payload exceeds 64 MB".into());
        }
        gz_name = None;
        data = bytes.to_vec();
    }
    let mut files = Vec::new();
    if data.len() > 262 && &data[257..262] == b"ustar" {
        let mut ar = tar::Archive::new(&data[..]);
        let entries = ar.entries().map_err(|e| format!("tar: {e}"))?;
        let mut total = 0usize;
        for e in entries {
            let Ok(e) = e else { break };
            if !e.header().entry_type().is_file() {
                continue;
            }
            let Ok(p) = e.path() else { continue };
            let Some(name) = safe_name(&p) else { continue };
            if !is_tex_source(&name) && !ext(&name).eq_ignore_ascii_case("bbl") {
                continue;
            }
            let mut buf = Vec::with_capacity((e.size() as usize).min(MAX_FILE).min(data.len()) + 1);
            if e.take(MAX_FILE as u64 + 1).read_to_end(&mut buf).is_err() || buf.len() > MAX_FILE {
                continue;
            }
            total += buf.len();
            if total > MAX_TOTAL {
                return Err("decompressed size exceeds 64 MB".into());
            }
            files.push((name, decode(buf)));
        }
        if files.is_empty() {
            return Err("no .tex files in archive".into());
        }
    } else {
        if data.starts_with(b"%PDF") || data[..data.len().min(8192)].contains(&0) {
            return Err("payload is not TeX source".into());
        }
        if data.len() > MAX_FILE {
            return Err("file exceeds 16 MB".into());
        }
        let name = gz_name
            .filter(|n| !n.is_empty() && !n.contains("..") && !n.starts_with('/'))
            .unwrap_or_else(|| "main.tex".into());
        files.push((name, decode(data)));
    }
    Ok(files)
}

/// Archive member name relative to the archive root; None for absolute or `..` paths.
fn safe_name(p: &std::path::Path) -> Option<String> {
    use std::path::Component::*;
    if let Some(s) = p.to_str().filter(|s| is_normal(s)) {
        return Some(s.to_string());
    }
    let mut parts = Vec::new();
    for c in p.components() {
        match c {
            Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// No empty, `.` or `..` segment and no leading `/`: already in the form `norm_path` produces.
fn is_normal(p: &str) -> bool {
    !p.is_empty() && !p.contains('\\') && p.split('/').all(|seg| !matches!(seg, "" | "." | ".."))
}

/// LaTeX source extensions seen in arXiv submissions.
fn is_tex_source(name: &str) -> bool {
    let e = ext(name);
    ["tex", "ltx", "latex"].iter().any(|x| e.eq_ignore_ascii_case(x))
}

fn ext(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((_, e)) if !e.contains('/') => e,
        _ => "",
    }
}

/// UTF-8, falling back to Latin-1 when invalid sequences outnumber valid non-ASCII chars.
fn decode(buf: Vec<u8>) -> String {
    match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => {
            let bytes = e.into_bytes();
            let lossy = String::from_utf8_lossy(&bytes);
            let bad = lossy.matches('\u{FFFD}').count();
            let good = lossy.chars().filter(|c| !c.is_ascii() && *c != '\u{FFFD}').count();
            if bad > good {
                bytes.iter().map(|&b| b as char).collect()
            } else {
                lossy.into_owned()
            }
        }
    }
}

// ---------------------------------------------------------------- masking / comments

#[derive(Clone, Copy, PartialEq)]
enum Dead {
    Comment,
    Verb,
}

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

/// Non-zero iff some byte of `v` equals `c`; the lowest set bit marks the first such byte.
#[inline(always)]
fn eq_mask(v: u64, c: u8) -> u64 {
    let x = v ^ (LO * c as u64);
    x.wrapping_sub(LO) & !x & HI
}

/// First position `>= from` whose byte is `c1` or `c2`, 8 bytes at a time.
#[inline]
fn find_either(b: &[u8], from: usize, c1: u8, c2: u8) -> Option<usize> {
    let s = b.get(from..)?;
    let mut chunks = s.chunks_exact(8);
    let mut at = from;
    for ch in &mut chunks {
        let v = u64::from_le_bytes(ch.try_into().unwrap());
        let m = eq_mask(v, c1) | eq_mask(v, c2);
        if m != 0 {
            return Some(at + (m.trailing_zeros() / 8) as usize);
        }
        at += 8;
    }
    chunks.remainder().iter().position(|&x| x == c1 || x == c2).map(|p| at + p)
}

/// Positions of every `%` and `\` in `b`: lets several scans share one pass over the text.
fn hit_positions(b: &[u8]) -> Vec<u32> {
    debug_assert!(b.len() < u32::MAX as usize, "tex source too large for u32 offsets");
    let mut v = Vec::with_capacity(b.len() / 32 + 16);
    let mut i = 0;
    while let Some(p) = find_either(b, i, b'%', b'\\') {
        v.push(p as u32);
        i = p + 1;
    }
    v
}

/// Walker over `hits` (non-decreasing queries) returning the first hit at or after `from`.
fn hit_walker(hits: &[u32]) -> impl FnMut(usize) -> Option<usize> + '_ {
    let mut k = 0;
    move |from| {
        while k < hits.len() && (hits[k] as usize) < from {
            k += 1;
        }
        hits.get(k).map(|&p| p as usize)
    }
}

/// Like `hit_walker`, but only backslashes still present in the mask `m`.
fn backslash_walker<'a>(hits: &'a [u32], m: &'a [u8]) -> impl FnMut(usize) -> Option<usize> + 'a {
    let mut walk = hit_walker(hits);
    move |mut from| loop {
        let p = walk(from)?;
        if m[p] == b'\\' {
            return Some(p);
        }
        from = p + 1;
    }
}

fn find_byte(b: &[u8], from: usize, c: u8) -> Option<usize> {
    find_either(b, from, c, c)
}

fn find_sub(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    let mut i = from;
    while let Some(p) = find_byte(b, i, pat[0]) {
        if b[p..].starts_with(pat) {
            return Some(p);
        }
        i = p + 1;
    }
    None
}

/// `\end{name}` at or after `from`: (start, end).
fn find_end(b: &[u8], from: usize, name: &[u8]) -> Option<(usize, usize)> {
    let mut i = from;
    while let Some(p) = find_byte(b, i, b'\\') {
        if let Some(r) = b[p + 1..].strip_prefix(b"end{").and_then(|r| r.strip_prefix(name)) {
            if r.first() == Some(&b'}') {
                return Some((p, p + 6 + name.len()));
            }
        }
        i = p + 1;
    }
    None
}

fn is_verb_env(n: &[u8]) -> bool {
    matches!(n, b"verbatim" | b"verbatim*" | b"Verbatim" | b"Verbatim*" | b"lstlisting" | b"minted")
}

const IF_PRIMS: [&[u8]; 20] = [
    b"iffalse", b"iftrue", b"ifx", b"ifnum", b"ifdim", b"ifcase", b"ifodd", b"ifvmode", b"ifhmode", b"ifmmode", b"ifinner", b"ifvoid",
    b"ifhbox", b"ifvbox", b"ifeof", b"ifcat", b"if", b"ifdefined", b"ifcsname", b"iffontchar",
];

/// For an `\iffalse` whose name ends at `from`: (end of the dead range, where scanning resumes).
/// Nested conditionals are balanced; an `\else` at depth 1 starts the live branch.
fn false_branch_end(b: &[u8], from: usize) -> (usize, usize) {
    let n = b.len();
    let mut depth = 1usize;
    let mut i = from;
    while i < n {
        if b[i] == b'%' {
            // a commented-out \fi / \else / \if is not part of the conditional structure
            i = find_byte(b, i, b'\n').unwrap_or(n);
            continue;
        }
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        let bs = i;
        i += 1;
        let s = i;
        while i < n && b[i].is_ascii_alphabetic() {
            i += 1;
        }
        if i == s {
            i += 1; // control symbol such as \% or \\
            continue;
        }
        let name = &b[s..i];
        if IF_PRIMS.contains(&name) {
            depth += 1;
        } else if name == b"fi" {
            depth -= 1;
            if depth == 0 {
                return (i, i);
            }
        } else if name == b"else" && depth == 1 {
            return (i, i);
        }
        let _ = bs;
    }
    (n, n)
}

/// For an `\iftrue` whose name ends at `from`: (start of its `\else`, end of the matching `\fi`).
fn true_else_range(b: &[u8], from: usize) -> Option<(usize, usize)> {
    let n = b.len();
    let (mut depth, mut i, mut else_at) = (1usize, from, None);
    while i < n {
        if b[i] == b'%' {
            i = find_byte(b, i, b'\n').unwrap_or(n);
            continue;
        }
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        let bs = i;
        i += 1;
        let s = i;
        while i < n && b[i].is_ascii_alphabetic() {
            i += 1;
        }
        if i == s {
            i += 1;
            continue;
        }
        let name = &b[s..i];
        if IF_PRIMS.contains(&name) {
            depth += 1;
        } else if name == b"else" && depth == 1 {
            else_at = Some(bs);
        } else if name == b"fi" {
            depth -= 1;
            if depth == 0 {
                return else_at.map(|e| (e, i));
            }
        }
    }
    None
}

/// Sorted, disjoint ranges that are comments (`%..`, `comment` env) or verbatim bodies.
fn dead_ranges(t: &str) -> Vec<(usize, usize, Dead)> {
    let b = t.as_bytes();
    dead_ranges_by(t, |i| find_either(b, i, b'%', b'\\'))
}

/// `dead_ranges` with `next(i)` giving the first `%` or `\` at or after `i`.
fn dead_ranges_by(t: &str, mut next: impl FnMut(usize) -> Option<usize>) -> Vec<(usize, usize, Dead)> {
    let b = t.as_bytes();
    let n = b.len();
    let mut v = Vec::new();
    let mut i = 0;
    while i < n {
        match b[i] {
            b'%' => {
                let j = find_byte(b, i, b'\n').unwrap_or(n);
                v.push((i, j, Dead::Comment));
                i = j;
            }
            b'\\' => {
                let bs = i;
                i += 1;
                if i >= n {
                    break;
                }
                // only url, href, if*, begin and verb matter here; other names hold no `%` or `\`
                if !matches!(b[i], b'u' | b'h' | b'i' | b'b' | b'v') {
                    i += 1;
                    continue;
                }
                let s = i;
                while i < n && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let name = &b[s..i];
                if name == b"url" || name == b"href" {
                    // a literal URL argument may contain `%` (percent-encoding): not a comment
                    let j = skip_ws(b, i);
                    if b.get(j) == Some(&b'{') {
                        if let Some(e) = match_close(b, j, b'{', b'}', 2000) {
                            i = e + 1;
                            continue;
                        }
                    }
                }
                if name == b"iffalse" {
                    // statically false branch: dead up to its \else (live) or matching \fi
                    let (end, resume) = false_branch_end(b, i);
                    v.push((bs, end, Dead::Comment));
                    i = resume;
                    continue;
                }
                if name == b"iftrue" {
                    // the \else branch of a statically true conditional is never typeset
                    if let Some((else_at, fi_end)) = true_else_range(b, i) {
                        v.push((else_at, fi_end, Dead::Comment));
                    }
                    continue;
                }
                if name == b"begin" && i < n && b[i] == b'{' {
                    let Some(c) = find_byte(b, i, b'}') else { continue };
                    let env = &b[i + 1..c];
                    if is_verb_env(env) || env == b"comment" {
                        if let Some((e, q)) = find_end(b, c + 1, env) {
                            if env == b"comment" {
                                v.push((bs, q, Dead::Comment));
                                i = q;
                            } else {
                                v.push((c + 1, e, Dead::Verb));
                                i = e;
                            }
                        }
                    }
                } else if name == b"verb" {
                    let mut k = i;
                    if k < n && b[k] == b'*' {
                        k += 1;
                    }
                    if k < n && b[k].is_ascii_punctuation() {
                        let d = b[k];
                        let eol = find_byte(b, k + 1, b'\n').unwrap_or(n);
                        if let Some(e) = b[k + 1..eol].iter().position(|&x| x == d) {
                            v.push((k + 1, k + 1 + e, Dead::Verb));
                            i = k + 2 + e;
                        }
                    }
                }
            }
            _ => i = next(i).unwrap_or(n),
        }
    }
    // ranges found inside an already-dead \else branch are redundant: keep a sorted, disjoint list
    let key = |r: &(usize, usize, Dead)| (r.0, std::cmp::Reverse(r.1));
    if !v.is_sorted_by(|a, b| key(a) <= key(b)) {
        v.sort_by_key(key);
    }
    let mut w = 0;
    for r in 0..v.len() {
        if w > 0 && v[r].0 < v[w - 1].1 {
            continue;
        }
        v[w] = v[r];
        w += 1;
    }
    v.truncate(w);
    v
}

/// Same-length view of `t` with comments and verbatim bodies blanked (borrowed when nothing is blanked).
fn mask(t: &str) -> Cow<'_, [u8]> {
    mask_with(t, &dead_ranges(t))
}

fn mask_with<'a>(t: &'a str, dead: &[(usize, usize, Dead)]) -> Cow<'a, [u8]> {
    if dead.is_empty() {
        return Cow::Borrowed(t.as_bytes());
    }
    let mut m = t.as_bytes().to_vec();
    for &(a, b, _) in dead {
        m[a..b].fill(b' ');
    }
    Cow::Owned(m)
}

/// Remove `%` comments and `comment` environments (verbatim bodies untouched). Comment-only lines vanish entirely.
fn strip_comments(t: &str) -> Cow<'_, str> {
    if !t.contains('%') && !t.contains("\\begin{comment}") && !t.contains("\\iffalse") && !t.contains("\\iftrue") {
        return Cow::Borrowed(t);
    }
    let dead = dead_ranges(t);
    if !dead.iter().any(|r| r.2 == Dead::Comment) {
        return Cow::Borrowed(t);
    }
    let b = t.as_bytes();
    let mut out = String::with_capacity(t.len());
    let mut last = 0;
    for (a, e, kind) in dead {
        if kind != Dead::Comment {
            continue;
        }
        out.push_str(&t[last..a]);
        let keep = out.trim_end_matches([' ', '\t']).len();
        out.truncate(keep);
        last = e;
        if (out.is_empty() || out.ends_with('\n')) && b.get(e) == Some(&b'\n') {
            last = e + 1;
        }
    }
    out.push_str(&t[last..]);
    Cow::Owned(out)
}

// ---------------------------------------------------------------- scanning helpers

fn skip_ws(m: &[u8], mut i: usize) -> usize {
    while i < m.len() && m[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Index of the closer matching `m[open]`, scanning at most `lim` bytes.
fn match_close(m: &[u8], open: usize, o: u8, c: u8, lim: usize) -> Option<usize> {
    let end = m.len().min(open + lim);
    let mut d = 0usize;
    let mut i = open;
    while i < end {
        let x = m[i];
        if x == b'\\' {
            i += 2;
            continue;
        }
        if x == o {
            d += 1;
        } else if x == c {
            d -= 1;
            if d == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Matching `]` for the `[` at `open`, ignoring brackets nested inside `{}`.
fn match_bracket(m: &[u8], open: usize) -> Option<usize> {
    let end = m.len().min(open + TITLE_LIM);
    let mut depth = 0usize;
    let mut i = open + 1;
    while i < end {
        match m[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b']' if depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// `{..}` group at (or after whitespace from) `j`: (inner_start, inner_end, next).
fn group(m: &[u8], j: usize, lim: usize) -> Option<(usize, usize, usize)> {
    let j = skip_ws(m, j);
    (m.get(j) == Some(&b'{')).then_some(())?;
    let e = match_close(m, j, b'{', b'}', lim)?;
    Some((j + 1, e, e + 1))
}

fn opt(m: &[u8], j: usize) -> Option<(usize, usize, usize)> {
    let j = skip_ws(m, j);
    (m.get(j) == Some(&b'[')).then_some(())?;
    let e = match_bracket(m, j)?;
    Some((j + 1, e, e + 1))
}

const DEF_LIM: usize = 200_000;

/// End offset of a macro/environment definition starting right after its command name, so section or
/// theorem commands inside definition bodies are not mistaken for document structure.
fn skip_definition(m: &[u8], i: usize, name: &[u8]) -> usize {
    let try_skip = || -> Option<usize> {
        let mut j = i + usize::from(m.get(i) == Some(&b'*'));
        if matches!(name, b"def" | b"gdef" | b"edef" | b"xdef") {
            let k = m.get(j..j + 200.min(m.len().saturating_sub(j)))?.iter().position(|&c| c == b'{')?;
            return group(m, j + k, DEF_LIM).map(|g| g.2);
        }
        j = skip_ws(m, j);
        if m.get(j) == Some(&b'{') {
            j = group(m, j, 200)?.2;
        } else if m.get(j) == Some(&b'\\') {
            j += 1;
            while m.get(j).is_some_and(u8::is_ascii_alphabetic) {
                j += 1;
            }
            if j == i + 1 {
                j += 1;
            }
        } else {
            return None;
        }
        while let Some((_, _, nx)) = opt(m, j) {
            j = nx;
        }
        if name.ends_with(b"DocumentCommand") || name.ends_with(b"DocumentEnvironment") {
            // xparse: {\name}{argspec}{body}
            j = group(m, j, 200)?.2;
        }
        j = group(m, j, DEF_LIM)?.2;
        if name.ends_with(b"environment") || name.ends_with(b"DocumentEnvironment") {
            if let Some((_, _, nx)) = group(m, j, DEF_LIM) {
                j = nx;
            }
        }
        Some(j)
    };
    try_skip().filter(|&e| e > i).unwrap_or(i)
}

// ---------------------------------------------------------------- events

enum Ev<'a> {
    /// raw level: 0 part, 1 chapter, 2 section, 3 subsection, 4 subsubsection.
    Head { raw: u8, star: bool, pos: usize, title: (usize, usize) },
    Appendix,
    Begin { name: &'a str, after: usize },
    End { name: &'a str, pos: usize },
}

struct Def<'a> {
    env: &'a str,
    label: String,
    star: bool,
    share: Option<&'a str>,
    within: Option<u8>,
    /// `[x]` / `numberwithin=x` names a parent counter we do not model (e.g. another theorem).
    bad_parent: bool,
}

#[derive(Default)]
struct Scan<'a> {
    evs: Vec<Ev<'a>>,
    defs: Vec<Def<'a>>,
    within: Vec<(&'a str, &'a str)>,
}

fn level_of(name: &str) -> Option<u8> {
    match name.trim() {
        "chapter" => Some(1),
        "section" => Some(2),
        "subsection" => Some(3),
        "subsubsection" => Some(4),
        _ => None,
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Collect structure events and definitions from the masked source; `next(i)` gives the first backslash of
/// `m` at or after `i`.
fn scan<'a>(t: &'a str, m: &[u8], mut next: impl FnMut(usize) -> Option<usize>) -> Scan<'a> {
    let n = m.len();
    let mut sc = Scan::default();
    sc.evs.reserve(n / 1024 + 16);
    let mut i = 0;
    while i < n {
        match next(i) {
            Some(p) => i = p + 1,
            None => break,
        }
        if i >= n {
            break;
        }
        // first letters of every command handled below; other names are skipped unread
        if !matches!(m[i], b'a'..=b'e' | b'g' | b'n' | b'p' | b'r' | b's' | b'x' | b'D' | b'N' | b'P' | b'R') {
            i += 1;
            continue;
        }
        let s = i;
        while i < n && m[i].is_ascii_alphabetic() {
            i += 1;
        }
        let name = &m[s..i];
        match name {
            b"part" | b"chapter" | b"section" | b"subsection" | b"subsubsection" => {
                let raw = match name {
                    b"part" => 0,
                    b"chapter" => 1,
                    b"section" => 2,
                    b"subsection" => 3,
                    _ => 4,
                };
                let star = m.get(i) == Some(&b'*');
                let mut j = i + star as usize;
                if let Some((_, _, nx)) = opt(m, j) {
                    j = nx;
                }
                if let Some((a, e, nx)) = group(m, j, TITLE_LIM) {
                    sc.evs.push(Ev::Head { raw, star, pos: s - 1, title: (a, e) });
                    i = nx;
                }
            }
            b"newcommand" | b"renewcommand" | b"providecommand" | b"DeclareRobustCommand" | b"DeclareMathOperator" | b"newenvironment"
            | b"renewenvironment" | b"NewDocumentCommand" | b"RenewDocumentCommand" | b"DeclareDocumentCommand" | b"ProvideDocumentCommand"
            | b"NewDocumentEnvironment" | b"RenewDocumentEnvironment" | b"DeclareDocumentEnvironment" | b"ProvideDocumentEnvironment" | b"def" | b"gdef" | b"edef" | b"xdef" => {
                i = skip_definition(m, i, name);
            }
            b"appendix" => sc.evs.push(Ev::Appendix),
            b"begin" | b"end" => {
                if let Some((a, e, nx)) = group(m, i, 100) {
                    if let Some(nm) = t.get(a..e) {
                        let nm = nm.trim();
                        if name == b"begin" {
                            if nm == "appendices" {
                                sc.evs.push(Ev::Appendix);
                            }
                            sc.evs.push(Ev::Begin { name: nm, after: nx });
                        } else {
                            sc.evs.push(Ev::End { name: nm, pos: s - 1 });
                            if nm == "document" {
                                break; // nothing after \end{document} is typeset
                            }
                        }
                    }
                    i = nx;
                }
            }
            b"newtheorem" => {
                let star = m.get(i) == Some(&b'*');
                let j = i + star as usize;
                let Some((a, e, mut j)) = group(m, j, 100) else { continue };
                let Some(env) = t.get(a..e) else { continue };
                let mut share = None;
                if let Some((a, e, nx)) = opt(m, j) {
                    share = t.get(a..e).map(str::trim);
                    j = nx;
                }
                let Some((a, e, mut j)) = group(m, j, TITLE_LIM) else { continue };
                let label = clean_inline(t.get(a..e).unwrap_or(""));
                let (mut within, mut bad_parent) = (None, false);
                if let Some((a, e, nx)) = opt(m, j) {
                    within = t.get(a..e).and_then(level_of);
                    bad_parent = within.is_none();
                    j = nx;
                }
                sc.defs.push(Def { env: env.trim(), label, star, share, within, bad_parent });
                i = j;
            }
            b"declaretheorem" => {
                let star = m.get(i) == Some(&b'*');
                let mut j = i + star as usize;
                let mut o = "";
                if let Some((a, e, nx)) = opt(m, j) {
                    o = t.get(a..e).unwrap_or("");
                    j = nx;
                }
                let Some((a, e, nx)) = group(m, j, 400) else { continue };
                let (mut label, mut share, mut within, mut star, mut bad_parent) = (None, None, None, star, false);
                for kv in split_top(o, b',') {
                    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                    let v = v.trim().trim_matches(['{', '}']).trim();
                    match k.trim() {
                        "name" => label = Some(clean_inline(v)),
                        "numberwithin" | "parent" => {
                            within = level_of(v);
                            bad_parent = within.is_none();
                        }
                        "sibling" | "sharenumber" | "numberlike" => share = Some(v),
                        "numbered" if v == "no" => star = true,
                        _ => {}
                    }
                }
                for env in t.get(a..e).unwrap_or("").split(',') {
                    let env = env.trim();
                    if env.is_empty() {
                        continue;
                    }
                    let label = label.clone().unwrap_or_else(|| capitalize(env));
                    // `share` borrows from `o`, which borrows from `t`; re-slice with the 'a lifetime.
                    sc.defs.push(Def { env, label, star, share, within, bad_parent });
                }
                i = nx;
            }
            b"numberwithin" => {
                if let Some((a, e, nx)) = group(m, i, 100) {
                    if let Some((a2, e2, nx2)) = group(m, nx, 100) {
                        if let (Some(x), Some(y)) = (t.get(a..e), t.get(a2..e2)) {
                            sc.within.push((x.trim(), y.trim()));
                        }
                        i = nx2;
                    }
                }
            }
            _ => {}
        }
    }
    sc
}

/// Split on `sep` at brace depth 0.
fn split_top(s: &str, sep: u8) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut v, mut d, mut st) = (Vec::new(), 0i32, 0);
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'{' => d += 1,
            b'}' => d -= 1,
            c if c == sep && d == 0 => {
                v.push(&s[st..i]);
                st = i + 1;
            }
            _ => {}
        }
    }
    v.push(&s[st..]);
    v
}

// ---------------------------------------------------------------- numbering

struct Num {
    c: [u32; 5],
    parts: u32,
    unnum: u32,
    app: bool,
    top: usize,
}

impl Num {
    fn new(evs: &[Ev]) -> Num {
        let chap = evs.iter().any(|e| matches!(e, Ev::Head { raw: 1, .. }));
        Num { c: [0; 5], parts: 0, unnum: 0, app: false, top: if chap { 1 } else { 2 } }
    }
    /// Step the counters for a heading and return its id.
    fn head(&mut self, raw: u8, star: bool) -> String {
        let mut id = String::with_capacity(12);
        self.head_into(raw, star, &mut id);
        id
    }
    /// `head`, replacing the contents of `id`.
    fn head_into(&mut self, raw: u8, star: bool, id: &mut String) {
        id.clear();
        if star {
            self.unnum += 1;
            let _ = write!(id, "s{}", self.unnum);
        } else if raw == 0 {
            self.parts += 1;
            let _ = write!(id, "P{}", self.parts);
        } else {
            let r = raw as usize;
            self.c[r] += 1;
            self.c[r + 1..].fill(0);
            self.prefix_into(r, id);
        }
    }
    fn appendix(&mut self) {
        self.app = true;
        self.c[self.top..].fill(0);
    }
    /// Number string of the current counters from the top level down to `r`.
    fn prefix_into(&self, r: usize, s: &mut String) {
        let first = s.len();
        for k in self.top.min(r)..=r {
            if s.len() > first {
                s.push('.');
            }
            let n = self.c[k];
            if self.app && k == self.top && (1..=26).contains(&n) {
                s.push((b'A' + n as u8 - 1) as char);
            } else {
                let _ = write!(s, "{n}");
            }
        }
    }
}

// ---------------------------------------------------------------- outline

/// Outline, theorem-like items and bibliography of `tex`, sharing one mask and one event scan.
pub fn analyze(tex: &str) -> (Vec<Section>, Vec<Item>, Vec<Ref>) {
    let hits = hit_positions(tex.as_bytes());
    let dead = dead_ranges_by(tex, hit_walker(&hits));
    let m = mask_with(tex, &dead);
    let sc = scan(tex, &m, backslash_walker(&hits, &m));
    (outline_of(tex, &sc), items_of(tex, &m, &sc), bib_refs(tex, &m, backslash_walker(&hits, &m)))
}

#[cfg(test)]
fn outline(tex: &str) -> Vec<Section> {
    analyze(tex).0
}

#[cfg(test)]
fn items(tex: &str) -> Vec<Item> {
    analyze(tex).1
}

#[cfg(test)]
fn refs(tex: &str) -> Vec<Ref> {
    analyze(tex).2
}

fn outline_of(tex: &str, sc: &Scan) -> Vec<Section> {
    let mut num = Num::new(&sc.evs);
    let mut present = [false; 5];
    for e in &sc.evs {
        if let Ev::Head { raw, .. } = e {
            present[*raw as usize] = true;
        }
    }
    let mut rank = [0u8; 5];
    let mut r = 0;
    for k in 0..5 {
        if present[k] {
            r += 1;
            rank[k] = r;
        }
    }
    let doc_end = sc
        .evs
        .iter()
        .rev()
        .find_map(|e| match e {
            Ev::End { name: "document", pos } => Some(*pos),
            _ => None,
        })
        .unwrap_or(tex.len());
    let heads = sc.evs.iter().filter(|e| matches!(e, Ev::Head { .. })).count();
    let mut secs: Vec<Section> = Vec::with_capacity(heads);
    let mut raws: Vec<u8> = Vec::with_capacity(heads);
    let mut open: Vec<usize> = Vec::new();
    for e in &sc.evs {
        match e {
            Ev::Appendix => num.appendix(),
            Ev::Head { raw, star, pos, title } => {
                while let Some(&k) = open.last() {
                    if raws[k] < *raw {
                        break;
                    }
                    secs[k].end = *pos;
                    open.pop();
                }
                let id = num.head(*raw, *star);
                secs.push(Section {
                    id,
                    level: rank[*raw as usize],
                    title: clean_inline(tex.get(title.0..title.1).unwrap_or("")),
                    start: *pos,
                    end: doc_end.max(*pos),
                });
                raws.push(*raw);
                open.push(secs.len() - 1);
            }
            _ => {}
        }
    }
    secs
}

// ---------------------------------------------------------------- items

/// Kind named by one lowercase word ("thm", "lemma", "problem" ...).
fn kind_word(w: &str) -> Option<Kind> {
    Some(match w {
        "theorem" | "thm" | "theorems" | "thrm" | "th" => Kind::Theorem,
        "lemma" | "lem" | "lemm" => Kind::Lemma,
        "proposition" | "prop" | "propn" | "prp" => Kind::Proposition,
        "corollary" | "cor" | "coro" | "corol" => Kind::Corollary,
        "conjecture" | "conj" | "cnj" => Kind::Conjecture,
        "problem" | "prob" => Kind::Problem,
        "question" | "ques" | "quest" => Kind::Question,
        "exercise" | "ex" | "exer" => Kind::Exercise,
        "definition" | "defn" | "def" | "dfn" | "defi" | "defin" => Kind::Definition,
        "remark" | "rem" | "rmk" => Kind::Remark,
        "example" | "exa" | "exm" | "exmp" => Kind::Example,
        "claim" | "cl" | "clm" => Kind::Claim,
        _ => return None,
    })
}

fn builtin_kind(env: &str) -> Option<Kind> {
    kind_word(env).or_else(|| {
        matches!(env, "observation" | "fact" | "hypothesis" | "notation" | "assumption" | "obs")
            .then_some(Kind::Other)
    })
}

fn kind_of_label(label: &str) -> Option<Kind> {
    label
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .find_map(|w| kind_word(&w.to_lowercase()))
}

fn kind_label(k: Kind, env: &str) -> Cow<'static, str> {
    Cow::Borrowed(match k {
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
        Kind::Other => return Cow::Owned(capitalize(env)),
    })
}

struct EnvInfo<'a> {
    ctr: Option<usize>,
    label: Cow<'a, str>,
    kind: Kind,
}

/// Theorem-like environments (honouring \newtheorem aliases).
/// The typeset part of a source: between `\begin{document}` and the real `\end{document}` (comments and
/// verbatim examples do not count); whole text when there are no markers.
pub fn body(tex: &str) -> &str {
    let dead = dead_ranges(tex);
    let b = tex.as_bytes();
    // a marker counts only when no byte of it lies in a comment or verbatim body
    let live = |from: usize, pat: &[u8]| {
        let mut i = from;
        while let Some(p) = find_sub(b, i, pat) {
            let k = dead.partition_point(|r| r.1 <= p);
            if dead.get(k).is_none_or(|r| r.0 >= p + pat.len()) {
                return Some(p);
            }
            i = p + 1;
        }
        None
    };
    let start = live(0, b"\\begin{document}").map_or(0, |p| p + 16);
    let end = live(start, b"\\end{document}").unwrap_or(tex.len());
    &tex[start..end]
}

/// Bibliography entries (`\bibitem`, including an inlined .bbl) as references.
/// `next(i)` gives the first backslash of `full` at or after `i`.
fn bib_refs(tex: &str, full: &[u8], mut next: impl FnMut(usize) -> Option<usize>) -> Vec<Ref> {
    // like outline/items, nothing after \end{document} is typeset
    let (mut starts, mut closes, mut end) = (Vec::new(), Vec::new(), full.len());
    let mut i = 0;
    while let Some(p) = next(i) {
        let r = &full[p..];
        if r.starts_with(b"\\end{document}") {
            end = p;
            break;
        } else if r.starts_with(b"\\bibitem") {
            starts.push(p);
        } else if r.starts_with(b"\\end{thebibliography}") {
            closes.push(p);
        }
        i = p + 1;
    }
    let tex = &tex[..end];
    let m = &full[..end];
    let mut out = Vec::with_capacity(starts.len());
    let mut links = String::new();
    for (k, &s) in starts.iter().enumerate() {
        // an entry ends at the next \bibitem or at the end of its own bibliography
        let end_all = closes.get(closes.partition_point(|&c| c < s)).copied().unwrap_or(tex.len());
        let mut j = s + 8;
        if let Some((_, _, nx)) = opt(m, j) {
            j = nx;
        }
        let Some((_, _, body)) = group(m, j, 200) else { continue };
        let e = starts.get(k + 1).copied().unwrap_or(end_all).min(end_all);
        if body >= e {
            continue;
        }
        let raw = &tex[body..e];
        let text = clean_inline(raw);
        if !text.is_empty() {
            // identifiers often live only in \href{https://doi.org/..}{label} / \url{..} targets
            links.clear();
            let rm = &m[body..e];
            for cmd in [&b"\\href"[..], &b"\\url"[..]] {
                let mut from = 0;
                while let Some(p) = find_sub(rm, from, cmd) {
                    let after = p + cmd.len();
                    if let Some((a, z, nx)) = group(rm, after, 2000) {
                        push_unescaped(&mut links, &raw[a..z]); // masked copy decides where the command is, raw gives the text
                        links.push(' ');
                        from = nx;
                    } else {
                        from = after;
                    }
                }
            }
            out.push(crate::html::make_ref(out.len() as u32 + 1, "", text, &links));
        }
    }
    out
}

/// Append `s` with `\_`, `\%`, `\&` and `\#` unescaped.
fn push_unescaped(out: &mut String, s: &str) {
    let mut rest = s;
    while let Some(p) = rest.find('\\') {
        out.push_str(&rest[..p]);
        match rest.as_bytes().get(p + 1) {
            Some(&c) if matches!(c, b'_' | b'%' | b'&' | b'#') => {
                out.push(c as char);
                rest = &rest[p + 2..];
            }
            _ => {
                out.push('\\');
                rest = &rest[p + 1..];
            }
        }
    }
    out.push_str(rest);
}

fn items_of(tex: &str, m: &[u8], sc: &Scan) -> Vec<Item> {
    let mut ctrs: Vec<(u32, Option<u8>)> = Vec::new();
    let mut envs: HashMap<&str, EnvInfo<'_>> = HashMap::new();
    for d in &sc.defs {
        let kind = kind_of_label(&d.label).or_else(|| builtin_kind(d.env)).unwrap_or(Kind::Other);
        let shared = d.share.and_then(|s| envs.get(s)).and_then(|e| e.ctr);
        let ctr = if d.star || d.bad_parent {
            None
        } else if shared.is_some() {
            shared
        } else if d.share.is_some() {
            // shares a counter we do not model (equation, section ...): omit the number rather than invent one
            None
        } else {
            ctrs.push((0, d.within));
            Some(ctrs.len() - 1)
        };
        let label = if d.label.is_empty() { Cow::Owned(capitalize(d.env)) } else { Cow::Borrowed(d.label.as_str()) };
        envs.insert(d.env, EnvInfo { ctr, label, kind });
    }
    for (a, b) in &sc.within {
        match level_of(b) {
            Some(level) => {
                if let Some(c) = envs.get(a).and_then(|e| e.ctr) {
                    ctrs[c].1 = Some(level);
                }
            }
            // numbered within another theorem (`\newtheorem{conj}{Conjecture}[thm]`): not modelled, so no number
            None => {
                if let Some(e) = envs.get_mut(a) {
                    e.ctr = None;
                }
            }
        }
    }

    // Pair each Begin with its End (innermost match; strays and unclosed begins are ignored).
    let mut pair = vec![usize::MAX; sc.evs.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (k, e) in sc.evs.iter().enumerate() {
        match e {
            Ev::Begin { .. } => stack.push(k),
            Ev::End { name, .. } => {
                let lo = stack.len().saturating_sub(32);
                if let Some(p) = (lo..stack.len()).rev().find(|&p| {
                    matches!(&sc.evs[stack[p]], Ev::Begin { name: bn, .. } if bn == name)
                }) {
                    pair[stack[p]] = k;
                    stack.truncate(p);
                }
            }
            _ => {}
        }
    }

    let mut num = Num::new(&sc.evs);
    let mut cur = String::new();
    let mut out = Vec::new();
    let mut scratch = Out::with_capacity(0);
    for (k, e) in sc.evs.iter().enumerate() {
        match e {
            Ev::Appendix => num.appendix(),
            Ev::Head { raw, star, .. } => {
                num.head_into(*raw, *star, &mut cur);
                if !star && *raw >= 1 {
                    for c in ctrs.iter_mut() {
                        if c.1.is_some_and(|w| *raw <= w) {
                            c.0 = 0;
                        }
                    }
                }
            }
            Ev::Begin { name, after } => {
                let (base, star) = match name.strip_suffix('*') {
                    Some(b) => (b, true),
                    None => (*name, false),
                };
                let (label, kind, ctr): (Cow<str>, Kind, Option<usize>) =
                    if let Some(i) = envs.get(*name).or_else(|| envs.get(base)) {
                        (Cow::Borrowed(&*i.label), i.kind, i.ctr)
                    } else if let Some(kd) = builtin_kind(base) {
                        (kind_label(kd, base), kd, None)
                    } else {
                        continue;
                    };
                // (count, `within` level) of the counter this item stepped
                let number = match ctr {
                    Some(c) if !star => {
                        ctrs[c].0 += 1;
                        Some((ctrs[c].0, ctrs[c].1))
                    }
                    _ => None,
                };
                let Some(Ev::End { pos: end, .. }) = sc.evs.get(pair[k]) else { continue };
                let mut b0 = *after;
                let mut title = String::new();
                if let Some((a, e, nx)) = opt(m, b0) {
                    if e < *end {
                        title = scratch.inline(tex.get(a..e).unwrap_or(""));
                        b0 = nx;
                    }
                }
                let text = clean(tex.get(b0..*end).unwrap_or(""));
                let keep = !text.is_empty();
                let mut full = String::new();
                if keep {
                    full.reserve(label.len() + title.len() + 16);
                    full.push_str(&label);
                    if let Some((n, within)) = number {
                        full.push(' ');
                        if let Some(w) = within {
                            num.prefix_into(w as usize, &mut full);
                            full.push('.');
                        }
                        let _ = write!(full, "{n}");
                    }
                    if !title.is_empty() {
                        full.push_str(" (");
                        full.push_str(&title);
                        full.push(')');
                    }
                }
                scratch.pool = title;
                if !keep {
                    continue;
                }
                let label = full;
                out.push(Item { kind, label, sec: cur.clone(), text });
            }
            Ev::End { .. } => {}
        }
    }
    out
}

// ---------------------------------------------------------------- clean

struct Out {
    s: String,
    /// spare buffer reused by `inline`, so repeated headings/titles do not allocate each time
    pool: String,
}

impl Out {
    fn with_capacity(n: usize) -> Out {
        Out { s: String::with_capacity(n), pool: String::new() }
    }
    /// `clean` of `s` with newlines turned into spaces. Hand the result back through `self.pool` when done.
    fn inline(&mut self, s: &str) -> String {
        let mut t = Out { s: std::mem::take(&mut self.pool), pool: String::new() };
        t.s.clear();
        run(s, &mut t);
        let mut b = t.s.into_bytes();
        for x in b.iter_mut().filter(|x| **x == b'\n') {
            *x = b' ';
        }
        String::from_utf8(b).unwrap_or_default()
    }
    fn space(&mut self) {
        if !matches!(self.s.as_bytes().last(), None | Some(b' ') | Some(b'\n')) {
            self.s.push(' ');
        }
    }
    fn newline(&mut self) {
        let keep = self.s.trim_end_matches(' ').len();
        self.s.truncate(keep);
        if !self.s.is_empty() && !self.s.ends_with('\n') {
            self.s.push('\n');
        }
    }
}

fn clean_inline(s: &str) -> String {
    Out::with_capacity(0).inline(s)
}

const MATH_ENVS: &[&str] = &[
    "equation", "equation*", "align", "align*", "alignat", "alignat*", "gather", "gather*", "multline",
    "multline*", "eqnarray", "eqnarray*", "displaymath", "flalign", "flalign*", "math",
];

/// Light LaTeX to text cleanup: math stays as `$..$` / `$$..$$`, markup is dropped.
pub fn clean(tex: &str) -> String {
    let mut o = Out::with_capacity(tex.len());
    run(tex, &mut o);
    o.s
}

/// Clean `tex` into `o.s`, which must be empty.
fn run(tex: &str, o: &mut Out) {
    let s = strip_comments(tex);
    let b = s.as_bytes();
    let n = b.len();
    o.s.reserve(n);
    let mut i = 0;
    while i < n {
        match b[i] {
            b' ' | b'\t' | b'\r' => {
                o.space();
                i += 1;
            }
            b'\n' => {
                let mut j = i + 1;
                let mut blank = false;
                while j < n && b[j].is_ascii_whitespace() {
                    blank |= b[j] == b'\n';
                    j += 1;
                }
                if blank {
                    o.newline();
                } else {
                    o.space();
                }
                i = j;
            }
            b'{' | b'}' => i += 1,
            b'~' => {
                o.space();
                i += 1;
            }
            b'`' | b'\'' => {
                if b.get(i + 1) == Some(&b[i]) {
                    o.s.push('"');
                    i += 2;
                } else {
                    o.s.push(b[i] as char);
                    i += 1;
                }
            }
            b'$' => {
                let dbl = b.get(i + 1) == Some(&b'$');
                let st = i + 1 + dbl as usize;
                match find_dollar(b, st, dbl) {
                    Some(e) if dbl => {
                        o.space();
                        o.s.push_str("$$");
                        math_copy(&s[st..e], o);
                        o.s.push_str("$$");
                        i = e + 2;
                        if !matches!(b.get(i), Some(b'.') | Some(b',') | Some(b';')) {
                            o.space();
                        }
                    }
                    Some(e) => {
                        o.s.push('$');
                        math_copy(&s[st..e], o);
                        o.s.push('$');
                        i = e + 1;
                    }
                    None => {
                        o.s.push_str(if dbl { "$$" } else { "$" });
                        i = st;
                    }
                }
            }
            b'\\' => i = command(&s, i, o),
            _ => {
                // copy plain text in one go; a lone space after a plain byte is what `space` would emit anyway
                let mut j = i + 1;
                while j + 1 < n && !STOP[b[j] as usize * 2 + IS_WS[b[j + 1] as usize] as usize] {
                    j += 1;
                }
                if j + 1 >= n {
                    while j < n && !SPECIAL[b[j] as usize] {
                        j += 1;
                    }
                }
                o.s.push_str(&s[i..j]);
                i = j;
            }
        }
    }
    let t = o.s.trim_end().len();
    o.s.truncate(t);
    let lead = o.s.len() - o.s.trim_start_matches('\n').len();
    o.s.drain(..lead);
}

/// `[byte * 2 + (next byte is whitespace)]`: ends a plain-text run. A space ends it only before more
/// whitespace; otherwise it is copied as-is, which is what `Out::space` would emit.
static STOP: [bool; 512] = {
    let mut t = [false; 512];
    let mut c = 0;
    while c < 256 {
        t[c * 2] = SPECIAL[c] && c != b' ' as usize;
        t[c * 2 + 1] = SPECIAL[c];
        c += 1;
    }
    t
};
static IS_WS: [bool; 256] = {
    let mut t = [false; 256];
    let sp = b" \t\r\n";
    let mut i = 0;
    while i < sp.len() {
        t[sp[i] as usize] = true;
        i += 1;
    }
    t
};
static SPECIAL: [bool; 256] = {
    let mut t = [false; 256];
    let sp = b" \t\r\n{}~`'$\\";
    let mut i = 0;
    while i < sp.len() {
        t[sp[i] as usize] = true;
        i += 1;
    }
    t
};

fn find_dollar(b: &[u8], mut i: usize, dbl: bool) -> Option<usize> {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'$' if !dbl || b.get(i + 1) == Some(&b'$') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Copy math verbatim, collapsing whitespace and dropping `\label{..}`, `\nonumber`, `\notag`.
#[allow(unused_assignments)]
fn math_copy(body: &str, o: &mut Out) {
    let body = body.trim();
    let b = body.as_bytes();
    let mut i = 0;
    let mut st = 0;
    let mut pending_space = false;
    macro_rules! flush {
        ($e:expr) => {
            if st < $e {
                if pending_space {
                    o.s.push(' ');
                    pending_space = false;
                }
                o.s.push_str(&body[st..$e]);
            }
        };
    }
    while i < b.len() {
        match b[i] {
            c if c.is_ascii_whitespace() => {
                flush!(i);
                let mut j = i;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                pending_space = true;
                i = j;
                st = j;
            }
            b'\\' => {
                let rest = &body[i..];
                let skip = if matches!(b.get(i + 1), Some(b'l' | b'n')) {
                    ["\\label", "\\nonumber", "\\notag"].iter().find_map(|p| {
                        let after = rest.get(p.len()..)?;
                        (rest.starts_with(p) && !after.starts_with(|c: char| c.is_ascii_alphabetic()))
                            .then_some(p.len())
                    })
                } else {
                    None
                };
                if let Some(l) = skip {
                    flush!(i);
                    let mut j = i + l;
                    if l == 6 {
                        if let Some((_, _, nx)) = group(b, j, 200) {
                            j = nx;
                        }
                    }
                    i = j;
                    st = j;
                } else {
                    i += 1;
                    if let Some(c) = body[i..].chars().next() {
                        i += c.len_utf8();
                    }
                }
            }
            _ => i += 1,
        }
    }
    flush!(b.len());
}

/// Handle the control sequence at `s[i]` (a backslash); returns the new index.
fn command(s: &str, i: usize, o: &mut Out) -> usize {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = i + 1;
    if i >= n {
        return n;
    }
    let c = b[i];
    if !c.is_ascii_alphabetic() {
        i += 1;
        match c {
            b'\\' => {
                o.newline();
                if b.get(i) == Some(&b'*') {
                    i += 1;
                }
                if let Some((_, _, nx)) = opt(b, i) {
                    i = nx;
                }
            }
            b'(' | b'[' => {
                let close: &[u8] = if c == b'(' { b"\\)" } else { b"\\]" };
                match find_sub(b, i, close) {
                    Some(e) => {
                        if c == b'[' {
                            o.space();
                        }
                        let d = if c == b'(' { "$" } else { "$$" };
                        o.s.push_str(d);
                        math_copy(&s[i..e], o);
                        o.s.push_str(d);
                        i = e + 2;
                        if c == b'[' && !matches!(b.get(i), Some(b'.') | Some(b',') | Some(b';')) {
                            o.space();
                        }
                    }
                    None => {}
                }
            }
            b'%' | b'$' | b'#' | b'_' | b'&' | b'{' | b'}' => o.s.push(c as char),
            b',' | b';' | b':' | b' ' | b'\n' => o.space(),
            b'\'' | b'`' | b'"' | b'^' | b'~' | b'=' | b'.' => {
                let (base, nx) = accent_base(s, i);
                if let Some(ch) = base {
                    o.s.push(compose(c as char, ch).unwrap_or(ch));
                }
                i = nx;
            }
            _ if c >= 0x80 => {
                if let Some(ch) = s[i - 1..].chars().next() {
                    i = i - 1 + ch.len_utf8();
                }
            }
            _ => {}
        }
        return i;
    }
    let st = i;
    while i < n && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    let name = &s[st..i];
    match name {
        "begin" => return env_begin(s, i, o),
        "end" => {
            if let Some((_, _, nx)) = group(b, i, 100) {
                i = nx;
            }
            o.newline();
        }
        "item" => {
            o.newline();
            o.s.push_str("- ");
            if let Some((a, e, nx)) = opt(b, i) {
                let t = o.inline(&s[a..e]);
                o.s.push_str(&t);
                o.pool = t;
                o.space();
                i = nx;
            }
        }
        "par" | "newline" => o.newline(),
        "part" | "chapter" | "section" | "subsection" | "subsubsection" | "paragraph" | "subparagraph" => {
            let mut j = i + (b.get(i) == Some(&b'*')) as usize;
            if let Some((_, _, nx)) = opt(b, j) {
                j = nx;
            }
            if let Some((a, e, nx)) = group(b, j, TITLE_LIM) {
                let title = o.inline(&s[a..e]);
                match name {
                    "part" | "chapter" => {
                        o.newline();
                        o.s.push_str("# ");
                    }
                    "section" => {
                        o.newline();
                        o.s.push_str("## ");
                    }
                    "subsection" => {
                        o.newline();
                        o.s.push_str("### ");
                    }
                    "subsubsection" => {
                        o.newline();
                        o.s.push_str("#### ");
                    }
                    _ => o.space(),
                }
                o.s.push_str(&title);
                o.pool = title;
                if matches!(name, "paragraph" | "subparagraph") {
                    o.space();
                } else {
                    o.newline();
                }
                i = nx;
            }
        }
        "c" | "v" | "u" | "H" if matches!(b.get(skip_ws(b, i)), Some(b'{') | Some(b'a'..=b'z') | Some(b'A'..=b'Z')) => {
            let (base, nx) = accent_base(s, i);
            if let Some(ch) = base {
                o.s.push(compose(name.as_bytes()[0] as char, ch).unwrap_or(ch));
            }
            i = nx;
        }
        "ss" => o.s.push('ß'),
        "o" => o.s.push('ø'),
        "O" => o.s.push('Ø'),
        "ae" => o.s.push('æ'),
        "AE" => o.s.push('Æ'),
        "oe" => o.s.push('œ'),
        "l" => o.s.push('ł'),
        "L" => o.s.push('Ł'),
        "aa" => o.s.push('å'),
        "i" => o.s.push('i'),
        "j" => o.s.push('j'),
        "S" => o.s.push('§'),
        "P" => o.s.push('¶'),
        "ldots" | "dots" | "textellipsis" => o.s.push_str("..."),
        "textbackslash" => o.s.push('\\'),
        "url" => {
            // URL text is literal: `~` and `%` must not be cleaned
            if let Some((a, e, nx)) = group(b, i, 2000) {
                o.s.push_str(s.get(a..e).unwrap_or(""));
                i = nx;
            }
        }
        "verb" => {
            // `\verb*|a b|`: the optional star precedes the real delimiter
            let star = usize::from(b.get(i) == Some(&b'*'));
            if let Some(&d) = b.get(i + star).filter(|d| d.is_ascii_punctuation()) {
                if let Some(e) = b.get(i + star + 1..).and_then(|r| r.iter().position(|&x| x == d)) {
                    o.s.push_str(&s[i + star + 1..i + star + 1 + e]);
                    i += star + e + 2;
                }
            }
        }
        _ => {
            if let Some((rep, spec)) = cmd_action(name) {
                if !rep.is_empty() {
                    o.s.push_str(rep);
                }
                i = skip_args(b, i, spec);
            }
        }
    }
    i
}

/// Replacement text and argument spec (`r` required, `o` optional, `p` macro params) for dropped commands.
fn cmd_action(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "cite" | "citep" | "citet" | "citealp" | "citeauthor" | "citeyear" | "parencite" | "textcite"
        | "citealt" | "Cite" | "Citep" | "Citet" => ("[cite]", "oor"),
        "bibitem" => ("\n", "or"),
        "ref" | "eqref" | "cref" | "Cref" | "autoref" | "pageref" | "vref" | "nameref" => ("?", "r"),
        "label" | "vspace" | "hspace" | "vskip" | "hskip" | "thispagestyle" | "pagestyle"
        | "pagenumbering" | "bibliographystyle" | "bibliography" | "theoremstyle" | "hypersetup"
        | "geometry" | "usetikzlibrary" | "graphicspath" | "includeonly" | "input" | "include" | "color"
        | "href" | "texorpdfstring" | "textcolor" => ("", "r"),
        "setlength" | "addtolength" | "setcounter" | "addtocounter" | "DeclareMathOperator"
        | "numberwithin" => ("", "rr"),
        "usepackage" | "RequirePackage" | "documentclass" | "includegraphics" | "footnote"
        | "footnotetext" | "marginpar" | "thanks" => ("", "or"),
        "hyperref" => ("", "o"),
        "newcommand" | "renewcommand" | "providecommand" => ("", "roor"),
        "newtheorem" => ("", "roro"),
        "declaretheorem" => ("", "or"),
        "newenvironment" | "renewenvironment" => ("", "roorr"),
        "def" => ("", "rpr"),
        "newcounter" => ("", "ro"),
        _ => return None,
    })
}

fn skip_args(b: &[u8], mut i: usize, spec: &str) -> usize {
    if b.get(i) == Some(&b'*') {
        i += 1;
    }
    for c in spec.bytes() {
        match c {
            b'o' => {
                if let Some((_, _, nx)) = opt(b, i) {
                    i = nx;
                }
            }
            b'r' => {
                let j = skip_ws(b, i);
                if let Some((_, _, nx)) = group(b, j, TITLE_LIM * 4) {
                    i = nx;
                } else if b.get(j) == Some(&b'\\') {
                    i = j + 1;
                    while i < b.len() && b[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                }
            }
            b'p' => {
                while b.get(i) == Some(&b'#') && b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                    i += 2;
                }
            }
            _ => {}
        }
    }
    i
}

fn env_begin(s: &str, i: usize, o: &mut Out) -> usize {
    let b = s.as_bytes();
    let Some((a, e, mut i)) = group(b, i, 100) else { return i };
    let name = s[a..e].trim();
    let base = name.trim_end_matches('*');
    let end_of = |from: usize| find_end(b, from, name.as_bytes());
    if is_verb_env(name.as_bytes()) {
        if let Some((p, q)) = end_of(i) {
            o.newline();
            o.s.push_str(s[i..p].trim_matches('\n'));
            o.newline();
            return q;
        }
    } else if name == "tikzpicture" {
        if let Some((_, q)) = end_of(i) {
            return q;
        }
    } else if MATH_ENVS.contains(&name) {
        if let Some((p, q)) = end_of(i) {
            o.space();
            o.s.push_str("$$");
            // `alignat{2}`: the column-count argument is layout, not equation content
            let st = if name.starts_with("alignat") || name.starts_with("xalignat") { group(b, i, 20).map_or(i, |g| g.2) } else { i };
            math_copy(&s[st..p], o);
            o.s.push_str("$$");
            if !matches!(b.get(q), Some(b'.') | Some(b',') | Some(b';')) {
                o.space();
            }
            return q;
        }
    }
    o.newline();
    let mut title = None;
    if let Some((a, e, nx)) = opt(b, i) {
        title = Some(o.inline(&s[a..e]));
        i = nx;
    }
    match base {
        "proof" => {
            o.s.push_str(title.as_deref().filter(|t| !t.is_empty()).unwrap_or("Proof"));
            o.s.push_str(". ");
        }
        "abstract" => o.s.push_str("Abstract. "),
        "tabular" | "tabularx" | "array" | "minipage" | "multicols" | "thebibliography" => {
            if let Some((_, _, nx)) = group(b, i, 400) {
                i = nx;
            }
        }
        _ => {
            if let Some(k) = builtin_kind(base) {
                o.s.push_str(&kind_label(k, base));
                if let Some(t) = title.as_deref().filter(|t| !t.is_empty()) {
                    o.s.push_str(" (");
                    o.s.push_str(t);
                    o.s.push(')');
                }
                o.s.push_str(". ");
            }
        }
    }
    if let Some(t) = title {
        o.pool = t;
    }
    i
}

/// Base letter of an accent command: `\'e`, `\' e`, `\'{e}`, `\'{\i}`; returns (letter, new index).
fn accent_base(s: &str, i: usize) -> (Option<char>, usize) {
    let b = s.as_bytes();
    let mut j = skip_ws(b, i);
    let braced = b.get(j) == Some(&b'{');
    if braced {
        j += 1;
    }
    let mut r = None;
    if b.get(j) == Some(&b'\\') && matches!(b.get(j + 1), Some(b'i') | Some(b'j')) {
        r = Some(b[j + 1] as char);
        j += 2;
    } else if let Some(c) = s.get(j..).and_then(|x| x.chars().next()).filter(|c| c.is_alphabetic()) {
        r = Some(c);
        j += c.len_utf8();
    }
    if r.is_some() && braced {
        if b.get(j) == Some(&b'}') {
            j += 1;
        } else {
            return (r, j);
        }
    }
    (r, if r.is_some() { j } else { i })
}

fn compose(acc: char, base: char) -> Option<char> {
    let (from, to) = match acc {
        '\'' => ("aeiouyAEIOUYcnszCNSZ", "áéíóúýÁÉÍÓÚÝćńśźĆŃŚŹ"),
        '`' => ("aeiouAEIOU", "àèìòùÀÈÌÒÙ"),
        '"' => ("aeiouyAEIOU", "äëïöüÿÄËÏÖÜ"),
        '^' => ("aeiouAEIOU", "âêîôûÂÊÎÔÛ"),
        '~' => ("anoANO", "ãñõÃÑÕ"),
        'c' => ("cCsStT", "çÇşŞţŢ"),
        'v' => ("cCsSzZrReEnNdD", "čČšŠžŽřŘěĚňŇďĎ"),
        'u' => ("aAgGeEoO", "ăĂğĞĕĔŏŎ"),
        'H' => ("oOuU", "őŐűŰ"),
        _ => return None,
    };
    from.chars().position(|c| c == base).and_then(|p| to.chars().nth(p))
}

// ---------------------------------------------------------------- flatten

fn norm_path(p: &str) -> String {
    if is_normal(p) {
        return p.to_string();
    }
    let mut v: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                v.pop();
            }
            s => v.push(s),
        }
    }
    v.join("/")
}

fn dir_of(p: &str) -> &str {
    p.rsplit_once('/').map(|x| x.0).unwrap_or("")
}

struct Flat<'a> {
    /// `\includeonly{a,b}` allow-list (names without extension); `None` = include everything.
    only: Option<Vec<String>>,
    files: &'a [(String, String)],
    stripped: Vec<Cow<'a, str>>,
    index: HashMap<String, usize>,
    base: String,
    main_stem: String,
    /// scratch path buffer for `resolve`
    path: std::cell::RefCell<String>,
}

impl Flat<'_> {
    /// Index of the file at `dir/name[.ext]` after normalisation.
    fn lookup(&self, dir: &str, name: &str, ext: Option<&str>) -> Option<usize> {
        let mut buf = self.path.borrow_mut();
        buf.clear();
        if !dir.is_empty() {
            buf.push_str(dir);
            buf.push('/');
        }
        buf.push_str(name);
        if let Some(e) = ext {
            buf.push('.');
            buf.push_str(e);
        }
        let hit = if is_normal(&buf) { self.index.get(buf.as_str()) } else { self.index.get(&norm_path(&buf)) };
        hit.copied()
    }

    /// Find a file for an \input-like argument: relative to the main dir, the including file, or by basename.
    fn resolve(&self, name: &str, cur_dir: &str, ext: &str) -> Option<usize> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        for e in [None, Some(ext)] {
            for dir in ["", self.base.as_str(), cur_dir] {
                if let Some(k) = self.lookup(dir, name, e) {
                    return Some(k);
                }
            }
        }
        fn base(p: &str) -> &str {
            p.rsplit('/').next().unwrap_or(p)
        }
        let a = base(name);
        // basename of `name.ext`
        let hit = |f: &str| {
            f == a || f.strip_prefix(a).and_then(|r| r.strip_prefix('.')).is_some_and(|r| r == ext)
        };
        let mut hits = self.files.iter().enumerate().filter(|(_, (n, _))| hit(base(n)));
        match (hits.next(), hits.next()) {
            (Some((k, _)), None) => Some(k),
            _ => None,
        }
    }

    fn expand(&self, idx: usize, text: &str, depth: usize, stack: &mut Vec<usize>, out: &mut String) {
        stack.push(idx);
        // `\endinput` stops reading this file (after the rest of its line)
        let mut hits = hit_positions(text.as_bytes());
        let dead = dead_ranges_by(text, hit_walker(&hits));
        let m0 = mask_with(text, &dead);
        let endinput = {
            let mut walk = backslash_walker(&hits, &m0);
            let mut at = 0;
            loop {
                match walk(at) {
                    Some(p) if m0[p..].starts_with(b"\\endinput") => break Some(p),
                    Some(p) => at = p + 1,
                    None => break None,
                }
            }
        };
        let (text, m) = match endinput {
            Some(p) => {
                let text = &text[..find_byte(&m0, p, b'\n').unwrap_or(text.len())];
                hits = hit_positions(text.as_bytes());
                (text, mask(text))
            }
            None => (text, m0),
        };
        let n = m.len();
        let cur_dir = dir_of(&self.files[idx].0);
        let (mut last, mut i) = (0, 0);
        let mut walk = backslash_walker(&hits, &m);
        while let Some(bs) = walk(i) {
            i = bs + 1;
            let s = i;
            while i < n && m[i].is_ascii_alphabetic() {
                i += 1;
            }
            let is_include = &m[s..i] == b"include";
            let (ext, subfile) = match &m[s..i] {
                b"input" | b"include" => ("tex", false),
                b"subfile" => ("tex", true),
                b"bibliography" => ("bbl", false),
                _ => continue,
            };
            let (arg, next) = match group(&m, i, 1000) {
                Some((a, e, nx)) => (&text[a..e], nx),
                None if m[s..i] == *b"input" => {
                    let j = skip_ws(&m, i);
                    let mut e = j;
                    while e < n && !m[e].is_ascii_whitespace() && !matches!(m[e], b'\\' | b'{' | b'}') {
                        e += 1;
                    }
                    (&text[j..e], e)
                }
                None => continue,
            };
            if is_include {
                if let Some(only) = &self.only {
                    let arg_stem = arg.trim().trim_end_matches(".tex");
                    if !only.iter().any(|o| o == arg_stem) {
                        out.push_str(&text[last..bs]);
                        last = next;
                        i = next;
                        continue;
                    }
                }
            }
            let target = if ext == "bbl" {
                let first = arg.split(',').next().unwrap_or("");
                self.resolve(first, cur_dir, "bbl")
                    .or_else(|| self.index.get(&format!("{}.bbl", self.main_stem)).copied())
            } else {
                self.resolve(arg, cur_dir, "tex")
            };
            let Some(t) = target else { continue };
            if depth >= MAX_DEPTH || stack.contains(&t) || out.len() > MAX_TOTAL {
                continue;
            }
            let mut body: &str = &self.stripped[t];
            if subfile {
                if let (Some(a), Some(e)) = (body.find("\\begin{document}"), body.rfind("\\end{document}")) {
                    if a + 16 <= e {
                        body = &body[a + 16..e];
                    }
                }
            }
            out.push_str(&text[last..bs]);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            self.expand(t, body, depth + 1, stack, out);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            last = next;
            i = next;
        }
        out.push_str(&text[last..]);
        stack.pop();
    }
}

/// Pick main file (has \documentclass / \begin{document}) and inline \input/\include (depth-limited).
pub fn flatten(files: &[(String, String)]) -> Option<String> {
    let texs: Vec<usize> = (0..files.len()).filter(|&k| is_tex_source(&files[k].0)).collect();
    if texs.is_empty() {
        return None;
    }
    let stripped: Vec<Cow<str>> = files.iter().map(|(_, c)| strip_comments(c)).collect();
    let pick = |pats: &[&str]| -> Vec<usize> {
        texs.iter().copied().filter(|&k| pats.iter().all(|p| stripped[k].contains(p))).collect()
    };
    let mut cands = pick(&["\\documentclass", "\\begin{document}"]);
    for pats in [&["\\begin{document}"][..], &["\\documentclass"][..]] {
        if cands.is_empty() {
            cands = pick(pats);
        }
    }
    if cands.is_empty() {
        cands = texs.clone();
    }
    let stem = |k: usize| {
        let n = &files[k].0;
        let f = n.rsplit('/').next().unwrap_or(n);
        f.rsplit_once('.').map(|x| x.0).unwrap_or(f).to_lowercase()
    };
    // `\documentclass[../article.tex]{subfiles}` chapters are parts of a paper, never the root
    let is_subfile = |k: usize| {
        let s = &stripped[k];
        s.find("\\documentclass").is_some_and(|p| {
            let rest = s[p + 14..].trim_start();
            let rest = match rest.strip_prefix('[') {
                Some(r) => r.split_once(']').map_or("", |x| x.1),
                None => rest,
            };
            rest.trim_start().starts_with("{subfiles}")
        })
    };
    let roots: Vec<usize> = cands.iter().copied().filter(|&k| !is_subfile(k)).collect();
    let cands = if roots.is_empty() { cands } else { roots };
    let named: Vec<usize> =
        cands.iter().copied().filter(|&k| matches!(stem(k).as_str(), "main" | "paper" | "ms" | "arxiv")).collect();
    let pool = if named.is_empty() { cands } else { named };
    let main = pool.iter().copied().fold(pool[0], |best, k| if stripped[k].len() > stripped[best].len() { k } else { best });
    let index = files.iter().enumerate().map(|(k, (n, _))| (norm_path(n), k)).collect();
    let only = stripped[main].find("\\includeonly").and_then(|p| {
        let m = mask(&stripped[main]);
        group(&m, p + 12, 2000).map(|(a, e, _)| stripped[main][a..e].split(',').map(|x| x.trim().trim_end_matches(".tex").to_string()).collect())
    });
    let fl = Flat {
        only,
        files,
        index,
        base: dir_of(&norm_path(&files[main].0)).to_string(),
        main_stem: {
            let n = norm_path(&files[main].0);
            n.rsplit_once('.').map(|x| x.0.to_string()).unwrap_or(n)
        },
        stripped,
        path: Default::default(),
    };
    let mut out = String::with_capacity(fl.stripped.iter().map(|s| s.len()).sum::<usize>().min(MAX_TOTAL));
    fl.expand(main, &fl.stripped[main], 0, &mut Vec::new(), &mut out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fx(n: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{n}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn targz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tb = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tb.append(&h, *data).unwrap();
        }
        gz(&tb.into_inner().unwrap())
    }

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn f(name: &str, body: &str) -> (String, String) {
        (name.to_string(), body.to_string())
    }

    fn ids(secs: &[Section]) -> Vec<(&str, u8, &str)> {
        secs.iter().map(|s| (s.id.as_str(), s.level, s.title.as_str())).collect()
    }

    fn labels(it: &[Item]) -> Vec<(Kind, &str, &str, &str)> {
        it.iter().map(|i| (i.kind, i.label.as_str(), i.sec.as_str(), i.text.as_str())).collect()
    }

    // ------------------------------------------------------------ unpack

    #[test]
    fn unpack_single_gz_fixture() {
        let files = unpack(&fx("tex_single.gz")).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "Features_of_a_high_school_olympiad_problem.tex");
        assert_eq!(files[0].1.len(), 31861);
        assert!(files[0].1.starts_with("\\documentclass[12 pt]{amsart}"));
    }

    #[test]
    fn unpack_multi_tar_fixture_keeps_only_tex() {
        let files = unpack(&fx("tex_multi.tar.gz")).unwrap();
        let mut names: Vec<&str> = files.iter().map(|f| f.0.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "main.tex",
                "modules.tex",
                "sections/section1.tex",
                "sections/section2.tex",
                "sections/section3.tex",
                "sections/section4.tex",
                "sections/section5.tex",
                "sections/section6.tex",
                "sections/section7.tex",
                "sections/sectionA.tex"
            ]
        );
    }

    #[test]
    fn unpack_tar_skips_pdfs() {
        let files = unpack(&fx("tex_tar_with_pdfs.tar.gz")).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "USAMO2020toMathMag.tex");
        assert_eq!(files[0].1.len(), 27685);
    }

    #[test]
    fn unpack_raw_text_and_junk() {
        let files = unpack(b"\\documentclass{article}\\begin{document}hi\\end{document}").unwrap();
        assert_eq!(files, vec![f("main.tex", "\\documentclass{article}\\begin{document}hi\\end{document}")]);
        assert!(unpack(b"").is_err());
        assert!(unpack(b"%PDF-1.5\n\0\0binary").is_err());
        assert!(unpack(&[0x1f, 0x8b, 1, 2, 3]).is_err());
    }

    #[test]
    fn unpack_tar_rejects_traversal_and_other_extensions() {
        let tgz = targz(&[
            ("a.tex", b"A"),
            ("../evil.tex", b"X"),
            ("/abs.tex", b"X"),
            ("img.png", b"\x89PNG"),
            ("./sub/b.bbl", b"B"),
            ("sub/../../c.tex", b"X"),
            ("UP.TEX", b"U"),
        ]);
        let files = unpack(&tgz).unwrap();
        assert_eq!(files, vec![f("a.tex", "A"), f("sub/b.bbl", "B"), f("UP.TEX", "U")]);
    }

    #[test]
    fn unpack_tar_without_tex_is_error() {
        assert_eq!(unpack(&targz(&[("fig.pdf", b"x")])), Err("no .tex files in archive".into()));
    }

    #[test]
    fn unpack_zip_bomb_guard() {
        let bomb = gz(&vec![b'a'; 65 << 20]);
        assert!(bomb.len() < 1 << 20);
        assert_eq!(unpack(&bomb), Err("decompressed size exceeds 64 MB".into()));
        let tgz = targz(&[("big.tex", &vec![b'a'; 17 << 20]), ("ok.tex", b"fine")]);
        assert_eq!(unpack(&tgz).unwrap(), vec![f("ok.tex", "fine")]);
    }

    #[test]
    fn unpack_decodes_latin1_and_lossy() {
        let files = unpack(b"caf\xe9 na\xefve \xfc \xe9\xe8").unwrap();
        assert_eq!(files[0].1, "café naïve ü éè");
        let mut mostly = "é".repeat(20).into_bytes();
        mostly.push(0xff);
        let files = unpack(&mostly).unwrap();
        assert_eq!(files[0].1, format!("{}\u{FFFD}", "é".repeat(20)));
    }

    // ------------------------------------------------------------ flatten

    #[test]
    fn flatten_multi_fixture_inlines_sections() {
        let flat = flatten(&unpack(&fx("tex_multi.tar.gz")).unwrap()).unwrap();
        assert!(!flat.contains("\\input{"));
        assert!(flat.contains("\\newenvironment{theorem}"));
        let a = flat.find("\\section{Introduction}").unwrap();
        let b = flat.find("\\section{Related Work}").unwrap();
        assert!(a < b);
        assert!(flat.contains("\\begin{thebibliography}"));
        assert!(!flat.contains("title.tex"), "comment-hidden \\input must not be inlined");
    }

    #[test]
    fn flatten_picks_main_and_resolves_relative_names() {
        let files = [
            f("big.tex", "\\documentclass{article}\\begin{document}BIG BIG BIG BIG BIG\\end{document}"),
            f("paper.tex", "\\documentclass{article}\n\\begin{document}\n\\input{sec/a}\n\\include{b.tex}\n\\input c\n\\end{document}"),
            f("sec/a.tex", "A-body \\input{sec/inner}"),
            f("sec/inner.tex", "INNER"),
            f("b.tex", "B-body 100\\% % hidden"),
            f("c.tex", "C-body"),
        ];
        let flat = flatten(&files).unwrap();
        assert_eq!(
            flat,
            "\\documentclass{article}\n\\begin{document}\nA-body \nINNER\n\nB-body 100\\%\n\nC-body\n\n\\end{document}"
        );
    }

    #[test]
    fn flatten_picks_largest_without_preferred_name_and_handles_none() {
        let files = [
            f("x.tex", "\\documentclass{article}\\begin{document}small\\end{document}"),
            f("y.tex", "\\documentclass{article}\\begin{document}the larger one\\end{document}"),
            f("z.tex", "fragment only, no class"),
        ];
        assert!(flatten(&files).unwrap().contains("the larger one"));
        assert_eq!(flatten(&[]), None);
        assert_eq!(flatten(&[f("refs.bbl", "\\bibitem{a} x")]), None);
        assert_eq!(flatten(&[f("frag.tex", "just text")]).unwrap(), "just text");
    }

    #[test]
    fn flatten_cycles_depth_subfile_and_bbl() {
        let cyc = [
            f("a.tex", "\\documentclass{article}\\begin{document}A \\input{b}\\end{document}"),
            f("b.tex", "B \\input{a} \\input{c}"),
            f("c.tex", "C"),
        ];
        let flat = flatten(&cyc).unwrap();
        assert!(flat.contains("\\input{a}"), "cycle edge left untouched: {flat}");
        assert_eq!(flat.matches("C").count(), 1);

        let mut chain = vec![f("main.tex", "\\documentclass{article}\\begin{document}\\input{n0}\\end{document}")];
        for k in 0..12 {
            chain.push(f(&format!("n{k}.tex"), &format!("L{k} \\input{{n{}}}", k + 1)));
        }
        let flat = flatten(&chain).unwrap();
        assert!(flat.contains("L7"));
        assert!(!flat.contains("L8"));
        assert!(flat.contains("\\input{n8}"));

        let sub = [
            f("main.tex", "\\documentclass{article}\\begin{document}X\\subfile{s}\\bibliography{refs}\\end{document}"),
            f("s.tex", "\\documentclass[main]{subfiles}\\begin{document}SUB\\end{document}"),
            f("refs.bbl", "\\begin{thebibliography}{1}\\bibitem{a} A\\end{thebibliography}"),
        ];
        let flat = flatten(&sub).unwrap();
        assert!(flat.contains("X\nSUB\n"));
        assert!(!flat.contains("subfiles"));
        assert!(flat.contains("\\bibitem{a} A"));
    }

    // ------------------------------------------------------------ outline

    #[test]
    fn outline_single_fixture() {
        let t = &flatten(&unpack(&fx("tex_single.gz")).unwrap()).unwrap();
        let o = outline(t);
        assert_eq!(
            ids(&o),
            [
                ("1", 1, "The Problem"),
                ("2", 1, "Countable and uncountable."),
                ("3", 1, "Are the rational numbers countable or uncountable?"),
                ("4", 1, "Properties of the sequence"),
                ("5", 1, "A sequence of Johannes Kepler"),
                ("6", 1, "A solution to Problem (?)."),
                ("7", 1, "Continued fraction representation of elements and base 2 representation of indices"),
                ("8", 1, "Extension to positive real continued fractions and a sequence indexed by the 2-adic integers."),
            ]
        );
        assert!(t[o[0].start..o[0].end].starts_with("\\section{The Problem}"));
        assert!(t[o[0].start..o[0].end].contains("\\end{problem}"));
        assert_eq!(o[0].end, o[1].start);
        assert!(t[o[7].start..o[7].end].trim_end().ends_with("\\end{document}") == false);
        assert!(o[7].end <= t.len());
    }

    #[test]
    fn outline_multi_fixture_numbering_and_appendix() {
        let t = &flatten(&unpack(&fx("tex_multi.tar.gz")).unwrap()).unwrap();
        let o = outline(t);
        assert_eq!(
            ids(&o),
            [
                ("1", 1, "Introduction"),
                ("2", 1, "Related Work"),
                ("3", 1, "Problem Curation"),
                ("3.1", 2, "Olympiads"),
                ("3.2", 2, "V. I. Arnold Work"),
                ("3.3", 2, "Problem Types"),
                ("s1", 3, "Problem Category and Source Breakdown"),
                ("3.3.1", 3, "Combinatorics"),
                ("3.3.2", 3, "Number Theory"),
                ("4", 1, "Evaluation"),
                ("5", 1, "Conclusion"),
                ("A", 1, "Appendix"),
                ("A.0.1", 3, "Detailed solution of Combinatorics problems"),
                ("A.0.2", 3, "Detailed solution of Number Theory problems"),
                ("A.0.3", 3, "Detailed solution of Graph Theory problems"),
                ("A.0.4", 3, "Detailed solution of Algebra problems"),
            ]
        );
        // section 3 spans its subsections; 3.3 ends where section 4 begins.
        assert_eq!(o[2].end, o[9].start);
        assert_eq!(o[5].end, o[9].start);
        assert_eq!(o[6].end, o[7].start);
        assert!(t[o[2].start..o[2].end].contains("\\subsection{Olympiads}"));
    }

    #[test]
    fn outline_titles_starred_appendix() {
        let t = "\\begin{document}\n\\section[Short]{On $\\{x\\}$ and {\\bf bold} sets\\label{s:a}}\ntext\n\\subsection*{Unnumbered}\n\\subsection{Sub A}\n\\section*{Starred top}\n\\section{Second}\n\\subsection{Sub B}\n\\appendix\n\\section{Proof of \\texorpdfstring{$a$}{a}}\n\\subsection{Detail}\n\\section{Other}\n\\end{document}\ntrailing";
        let o = outline(t);
        assert_eq!(
            ids(&o),
            [
                ("1", 1, "On $\\{x\\}$ and bold sets"),
                ("s1", 2, "Unnumbered"),
                ("1.1", 2, "Sub A"),
                ("s2", 1, "Starred top"),
                ("2", 1, "Second"),
                ("2.1", 2, "Sub B"),
                ("A", 1, "Proof of a"),
                ("A.1", 2, "Detail"),
                ("B", 1, "Other"),
            ]
        );
        assert!(t[o[0].start..o[0].end].starts_with("\\section[Short]"));
        assert_eq!(&t[o[1].start..o[1].end], "\\subsection*{Unnumbered}\n");
        assert_eq!(o[0].end, o[3].start);
        assert_eq!(&t[o[8].start..o[8].end], "\\section{Other}\n");
    }

    #[test]
    fn outline_parts_chapters_and_unicode() {
        let t = "\\part{P}\\chapter{C1 é}\\section{S}\\chapter{C2}\\section{T}\\subsection{U}";
        let o = outline(t);
        assert_eq!(
            ids(&o),
            [("P1", 1, "P"), ("1", 2, "C1 é"), ("1.1", 3, "S"), ("2", 2, "C2"), ("2.1", 3, "T"), ("2.1.1", 4, "U")]
        );
        assert_eq!(&t[o[1].start..o[1].end], "\\chapter{C1 é}\\section{S}");
        assert_eq!(o[0].end, t.len());
    }

    #[test]
    fn outline_ignores_comments_and_verbatim() {
        let t = "% \\section{Commented}\n\\begin{verbatim}\n\\section{Verb}\n\\end{verbatim}\n\\begin{comment}\n\\section{Block}\n\\end{comment}\n\\newcommand{\\mysec}{\\section}\n\\verb|\\section{inline}|\n\\section{Real} 50\\% % \\section{no}\n";
        assert_eq!(ids(&outline(t)), [("1", 1, "Real")]);
    }

    // ------------------------------------------------------------ items

    #[test]
    fn items_single_fixture() {
        let t = &flatten(&unpack(&fx("tex_single.gz")).unwrap()).unwrap();
        let it = items(t);
        let got: Vec<_> = it.iter().map(|i| (i.kind, i.label.as_str(), i.sec.as_str())).collect();
        assert_eq!(
            got,
            [
                (Kind::Problem, "Problem 1.1", "1"),
                (Kind::Other, "Properties 4.1", "4"),
                (Kind::Other, "Instructions 7.1", "7"),
                (Kind::Theorem, "Theorem 7.2", "7"),
                (Kind::Theorem, "Theorem 8.1", "8"),
                (Kind::Theorem, "Theorem 8.2", "8"),
                (Kind::Example, "Example 8.3", "8"),
            ]
        );
        assert_eq!(
            it[0].text,
            "The sequence $\\{ a_n\\}_{n=1}^\\infty$ satisfies $a_1 = 1$ and for $n\\ge 1$, $$a_{2n} = a_n + 1; \\quad a_{2n+1} = \\frac 1{a_{2n}}.$$ Prove that every positive rational number occurs in the sequence exactly once."
        );
    }

    #[test]
    fn items_multi_fixture_custom_envs_and_late_newtheorem() {
        let t = &flatten(&unpack(&fx("tex_multi.tar.gz")).unwrap()).unwrap();
        let it = items(t);
        let got: Vec<_> = it.iter().map(|i| (i.kind, i.label.as_str(), i.sec.as_str())).collect();
        assert_eq!(
            got,
            [
                (Kind::Lemma, "Lemma", "A.0.1"),
                (Kind::Proposition, "Proposition 1", "A.0.1"),
                (Kind::Corollary, "Corollary", "A.0.1"),
                (Kind::Proposition, "Proposition 2", "A.0.1"),
            ]
        );
        assert_eq!(it[0].text, "Isomorphism of trees preserves the level of a vertex.");
        assert_eq!(it[2].text, "For $k \\geq 3$ we have $$\\chi(k) \\geq 2^{2^{k-3}}.$$");
    }

    #[test]
    fn items_newtheorem_counters_sharing_and_resets() {
        let t = "\\newtheorem{thm}{Theorem}[section]\n\\newtheorem{lem}[thm]{Lemma}\n\\newtheorem*{rmk}{Remark}\n\\newtheorem{prob}{Open Problem}\n\\newtheorem{conj}{Conjecture}\n\\begin{document}\n\\section{A}\n\\begin{thm}[Cauchy] Let $x$. \\label{t}\\end{thm}\n\\begin{lem}\\emph{Small} lemma.\\end{lem}\n\\begin{rmk}A remark.\\end{rmk}\n\\begin{prob}Find all $n$.\\end{prob}\n\\section{B}\n\\begin{thm}Again.\\end{thm}\n\\begin{conj}Maybe \\cite{x}.\\end{conj}\n\\appendix\n\\section{C}\n\\begin{thm}App.\\end{thm}\n\\end{document}";
        assert_eq!(
            labels(&items(t)),
            [
                (Kind::Theorem, "Theorem 1.1 (Cauchy)", "1", "Let $x$."),
                (Kind::Lemma, "Lemma 1.2", "1", "Small lemma."),
                (Kind::Remark, "Remark", "1", "A remark."),
                (Kind::Problem, "Open Problem 1", "1", "Find all $n$."),
                (Kind::Theorem, "Theorem 2.1", "2", "Again."),
                (Kind::Conjecture, "Conjecture 1", "2", "Maybe [cite]."),
                (Kind::Theorem, "Theorem A.1", "A", "App."),
            ]
        );
    }

    #[test]
    fn items_numberwithin_declaretheorem_and_label_kinds() {
        let t = "\\newtheorem{claim}{Claim}\n\\numberwithin{claim}{section}\n\\declaretheorem[name={Open Problem},numberwithin=section]{op}\n\\declaretheorem[name=Question,sibling=op]{qq}\n\\declaretheorem[numbered=no]{hyp}\n\\newtheorem{fancy}{Main Theorem}\n\\newtheorem{task}{Task}\n\\newtheorem{prob}{Aufgabe}\n\\section{S}\n\\begin{claim}c1\\end{claim}\\begin{op}o1\\end{op}\\begin{qq}q1\\end{qq}\\begin{hyp}h\\end{hyp}\\begin{fancy}f\\end{fancy}\\begin{task}t\\end{task}\\begin{prob}p\\end{prob}";
        assert_eq!(
            labels(&items(t)),
            [
                (Kind::Claim, "Claim 1.1", "1", "c1"),
                (Kind::Problem, "Open Problem 1.1", "1", "o1"),
                (Kind::Question, "Question 1.2", "1", "q1"),
                (Kind::Other, "Hyp", "1", "h"),
                (Kind::Theorem, "Main Theorem 1", "1", "f"),
                (Kind::Other, "Task 1", "1", "t"),
                (Kind::Problem, "Aufgabe 1", "1", "p"),
            ]
        );
    }

    #[test]
    fn items_builtin_names_are_unnumbered() {
        let t = "\\section{X}\\begin{lemma}L\\end{lemma}\\begin{observation}O\\end{observation}\\begin{conj}C\\end{conj}\\begin{ex*}E\\end{ex*}\\begin{notanenv}N\\end{notanenv}";
        assert_eq!(
            labels(&items(t)),
            [
                (Kind::Lemma, "Lemma", "1", "L"),
                (Kind::Other, "Observation", "1", "O"),
                (Kind::Conjecture, "Conjecture", "1", "C"),
                (Kind::Exercise, "Exercise", "1", "E"),
            ]
        );
    }

    #[test]
    fn items_skip_comments_verbatim_and_unbalanced() {
        let t = "% \\begin{theorem} hidden \\end{theorem}\n\\begin{verbatim}\\begin{lemma}x\\end{lemma}\\end{verbatim}\n\\begin{comment}\\begin{theorem}c\\end{theorem}\\end{comment}\n\\begin{theorem}Real % \\end{theorem}\nbody\\end{theorem}\n\\begin{theorem}never closed\n\\begin{lemma}ok\\end{lemma}\n\\end{theorem}\n\\end{claim}\\begin{claim}\\end{claim}";
        let it = items(t);
        assert_eq!(
            labels(&it),
            [
                (Kind::Theorem, "Theorem", "", "Real body"),
                (Kind::Theorem, "Theorem", "", "never closed\nLemma. ok"),
                (Kind::Lemma, "Lemma", "", "ok"),
            ]
        );
    }

    #[test]
    fn items_nested_same_env_pairs_innermost() {
        let t = "\\begin{theorem}A \\begin{theorem}B\\end{theorem} C\\end{theorem}";
        let it = items(t);
        assert_eq!(it.len(), 2);
        assert_eq!(it[0].text, "A\nTheorem. B\nC");
        assert_eq!(it[1].text, "B");
    }

    // ------------------------------------------------------------ clean

    #[test]
    fn clean_lists_cites_refs_math() {
        assert_eq!(
            clean("Let $G$ be \\emph{finite}.\n\\begin{enumerate}\n\\item First \\cite{a}\n\\item[(b)] Second \\ref{x}\n\\end{enumerate}"),
            "Let $G$ be finite.\n- First [cite]\n- (b) Second ?"
        );
        assert_eq!(clean("\\(x\\) and \\[ y \\label{e}\n = z \\]."), "$x$ and $$y = z$$.");
        assert_eq!(clean("\\begin{align*}a&=b\\\\c&=d \\label{q}\\end{align*} so"), "$$a&=b\\\\c&=d$$ so");
        assert_eq!(clean("$$ a $$ and $b  +\n c$"), "$$a$$ and $b + c$");
        assert_eq!(clean("cost \\$5 and $unclosed"), "cost $5 and $unclosed");
    }

    #[test]
    fn clean_comments_and_escapes() {
        assert_eq!(clean("a % c\nb \\% d \\& e"), "a b % d & e");
        assert_eq!(clean("x\n% only comment\ny\n\n\n% c\nz"), "x y\nz");
    }

    #[test]
    fn clean_accents_quotes_and_dropped_commands() {
        assert_eq!(clean("B\\'ELA, Erd\\H{o}s, na\\\"ive, \\c{c}a, \\v Cech, {\\'\\i}, \\ss"), "BÉLA, Erdős, naïve, ça, Čech, í, ß");
        assert_eq!(clean("``quoted'' it's"), "\"quoted\" it's");
        assert_eq!(clean("\\vspace{2mm}Hello\\footnote{note} world\\newcommand{\\foo}[1]{#1}!"), "Hello world!");
        assert_eq!(clean("\\href{http://x}{link} \\texttt{code} {\\bf Bold}~text\\ldots"), "link code Bold text...");
    }

    #[test]
    fn clean_structure_and_envs() {
        assert_eq!(clean("\\section{Intro} Text \\subsection*{Sub} More \\paragraph{Note.} Para"), "## Intro\nText\n### Sub\nMore Note. Para");
        assert_eq!(clean("\\begin{proof}[Proof of Lemma 1] Trivial.\\end{proof}"), "Proof of Lemma 1. Trivial.");
        assert_eq!(clean("\\begin{tikzpicture}\\draw (0,0);\\end{tikzpicture}After"), "After");
        assert_eq!(clean("\\begin{verbatim}\n a % b\n\\end{verbatim}"), " a % b");
        assert_eq!(clean("\\begin{theorem}[Cauchy] X\\end{theorem}"), "Theorem (Cauchy). X");
        assert_eq!(clean("\\begin{tabular}{cc} a & b \\\\ c & d \\end{tabular}"), "a & b\nc & d");
        assert_eq!(clean(""), "");
    }

    // ------------------------------------------------------------ robustness / perf

    #[test]
    fn byte_search_helpers_cover_every_offset() {
        let mut t = vec![b'x'; 70];
        t[63] = b'%';
        t[64] = b'\\';
        for from in 0..=70 {
            let want = |c: &[u8]| (from..70).find(|&i| c.contains(&t[i]));
            assert_eq!(find_either(&t, from, b'%', b'\\'), want(b"%\\"), "from {from}");
            assert_eq!(find_byte(&t, from, b'\\'), want(b"\\"), "from {from}");
        }
        assert_eq!(find_byte(&t, 71, b'x'), None);
        let b = b"\\begin{verbatim}\\end{verbatim*}\\end{verbatim}!";
        assert_eq!(find_end(b, 0, b"verbatim"), Some((31, 45)));
        assert_eq!(find_end(b, 0, b"verbatim*"), Some((16, 31)));
        assert_eq!(find_end(b, 0, b"nope"), None);
    }

    #[test]
    fn push_unescaped_unescapes_only_the_four_link_escapes() {
        let mut s = String::from(">");
        push_unescaped(&mut s, r"a\_b\%c\&d\#e\\f\x\");
        assert_eq!(s, ">a_b%c&d#e\\\\f\\x\\");
    }

    #[test]
    fn normal_paths_are_recognised() {
        for p in ["a", "a/b.tex", "a.b/c"] {
            assert!(is_normal(p), "{p}");
            assert_eq!(norm_path(p), p);
        }
        for p in ["", "/a", "a/", "a//b", "./a", "a/./b", "a/../b", "a\\b"] {
            assert!(!is_normal(p), "{p}");
        }
        assert_eq!(norm_path("./a//b/../c/"), "a/c");
    }

    #[test]
    fn unpack_ignores_a_lying_gzip_size_hint() {
        let src = "\\documentclass{article}\n".repeat(50);
        let mut g = gz(src.as_bytes());
        assert_eq!(unpack(&g).unwrap()[0].1, src);
        let n = g.len();
        g[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(unpack(&g).is_err());
    }

    #[test]
    fn no_panic_on_truncation_and_garbage() {
        let src = "é\\section*[é]{é$é$}\\begin{theorem}[é]\\label{é} é % é\n\\end{theorem}\\newtheorem{é}[é]{é}[é]\\verb|é|\\begin{verbatim}é\\item[é]\\'é\\c{é}\\begin{comment}\\declaretheorem[name=é]{é}\\input é{";
        for k in 0..=src.len() {
            if src.is_char_boundary(k) {
                let s = &src[..k];
                let _ = (outline(s), items(s), clean(s), flatten(&[f("a.tex", s)]));
            }
        }
        let toks = [
            "\\", "{", "}", "[", "]", "%", "$", "$$", "\\begin{theorem}", "\\end{theorem}", "\\section{", "\\newtheorem{x}", "é", "\n", " ", "\\\\", "\\verb", "|", "\\item", "\\[", "\\]", "\\appendix", "*", "\\input{a}", "\\begin{comment}", "ab",
        ];
        let mut x: u64 = 0x2545F4914F6CDD1D;
        for _ in 0..3000 {
            let mut s = String::new();
            for _ in 0..40 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                s.push_str(toks[(x % toks.len() as u64) as usize]);
            }
            let _ = (outline(&s), items(&s), clean(&s), flatten(&[f("a.tex", &s), f("b.tex", &s)]));
        }
    }

    fn big_doc(bytes: usize) -> String {
        let block = "\\section{Block}\nSome text with $x^2$ and a % comment \\section{hidden}\n\\begin{theorem}[Name] For all $n$, $n^2\\ge n$. \\label{t}\\end{theorem}\n\\begin{equation}\na=b\n\\end{equation}\n\\subsection{Sub}\nMore filler text to bulk the paragraph up a bit, with \\emph{emphasis} and \\cite{k}.\n\\begin{lemma}A lemma.\\end{lemma}\n\\begin{proof} Trivial. \\end{proof}\n";
        let mut s = String::from("\\newtheorem{theorem}{Theorem}[section]\n\\newtheorem{lemma}[theorem]{Lemma}\n\\begin{document}\n");
        while s.len() < bytes {
            s.push_str(block);
        }
        s
    }

    #[test]
    fn large_document_counts() {
        let t = big_doc(300_000);
        let o = outline(&t);
        let it = items(&t);
        let blocks = o.iter().filter(|s| s.level == 1).count();
        assert_eq!(o.len(), blocks * 2);
        assert_eq!(it.len(), blocks * 2);
        assert_eq!(it[0].label, "Theorem 1.1 (Name)");
        assert_eq!(it[1].label, "Lemma 1.2");
        assert_eq!(it[2].label, "Theorem 2.1 (Name)");
    }

    #[test]
    #[ignore = "timing: run with --release -- --ignored --nocapture"]
    fn perf_outline_items_300kb() {
        let t = big_doc(300_000);
        let best = |f: &dyn Fn() -> usize| {
            (0..30)
                .map(|_| {
                    let t0 = std::time::Instant::now();
                    std::hint::black_box(f());
                    t0.elapsed().as_secs_f64() * 1000.0
                })
                .fold(f64::MAX, f64::min)
        };
        let m = best(&|| mask(&t).len());
        let o = best(&|| outline(&t).len());
        let i = best(&|| items(&t).len());
        let per = best(&|| outline(&t).len() + items(&t).len());
        println!("{} bytes: mask {m:.2} ms, outline {o:.2} ms, items {i:.2} ms, outline+items {per:.2} ms", t.len());
        assert!(per < 10.0);
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;

    #[test]
    fn macro_bodies_are_not_document_structure() {
        let t = r"\newcommand{\unused}{\section{Ghost}}\def\x{\subsection{Ghost2}}\newenvironment{e}{\section{Ghost3}}{}\begin{document}\section{Real}Body\end{document}";
        let o = outline(t);
        assert_eq!(o.iter().map(|s| (s.id.as_str(), s.title.as_str())).collect::<Vec<_>>(), [("1", "Real")]);
    }

    #[test]
    fn starred_custom_theorem_environment_resolves() {
        let t = r"\newtheorem*{result*}{Main Theorem}\begin{document}\begin{result*}Statement.\end{result*}\end{document}";
        let it = items(t);
        assert_eq!(it.len(), 1);
        assert_eq!(it[0].label, "Main Theorem");
        assert_eq!(it[0].text, "Statement.");
    }

    #[test]
    fn xparse_definition_bodies_skipped() {
        let t = r"\NewDocumentCommand{\unused}{}{\section{Ghost}}\begin{document}\section{Real}Body.\end{document}";
        assert_eq!(outline(t).iter().map(|s| (s.id.as_str(), s.title.as_str())).collect::<Vec<_>>(), [("1", "Real")]);
    }

    #[test]
    fn renewed_and_environment_xparse_definitions_skipped() {
        let t = r"\RenewDocumentCommand{\maketitle}{}{\section{G1}}\NewDocumentEnvironment{e}{}{\section{G2}}{\section{G3}}\begin{document}\section{Real}B.\end{document}";
        assert_eq!(outline(t).len(), 1);
    }

    #[test]
    fn nothing_after_end_document() {
        let t = r"\begin{document}\section{Real}Visible.\end{document}\section{Draft}\begin{problem}Discarded.\end{problem}";
        assert_eq!(outline(t).len(), 1);
        assert!(items(t).is_empty());
    }

    #[test]
    fn flatten_prefers_submission_root_over_main_dir() {
        let files = vec![
            ("src/main.tex".to_string(), r"\documentclass{article}\begin{document}\input{body}\end{document}".to_string()),
            ("body.tex".to_string(), r"\section{Real}".to_string()),
            ("src/body.tex".to_string(), r"\section{Draft}".to_string()),
        ];
        let flat = flatten(&files).unwrap();
        assert!(flat.contains("Real") && !flat.contains("Draft"), "{flat}");
    }

    #[test]
    fn theorem_sharing_unmodelled_counter_is_unnumbered() {
        let t = r"\numberwithin{equation}{section}\newtheorem{thm}[equation]{Theorem}\begin{document}\section{A}\begin{equation}x=0\end{equation}\begin{thm}Statement.\end{thm}\end{document}";
        assert_eq!(items(t)[0].label, "Theorem");
    }

    #[test]
    fn alignat_column_count_is_not_content() {
        assert_eq!(clean(r"\begin{alignat}{2}a&=b\end{alignat}"), "$$a&=b$$");
    }

    #[test]
    fn bibliography_text_is_readable() {
        let c = clean(r"\begin{thebibliography}{99}\bibitem{i1} A. One, Title.\bibitem[X(2020)]{i2} B. Two, Other.\end{thebibliography}");
        assert_eq!(c, "A. One, Title.\nB. Two, Other.");
    }

    #[test]
    fn body_is_the_typeset_part() {
        assert_eq!(body(r"\documentclass{x}\begin{document}Current text.\end{document}Old draft."), "Current text.");
        assert_eq!(body("no markers"), "no markers");
        assert_eq!(body(r"\begin{document}see \verb|\end{document}| here\end{document}tail"), r"see \verb|\end{document}| here");
    }

    #[test]
    fn endinput_stops_a_file() {
        let files = vec![
            ("main.tex".to_string(), r"\documentclass{article}\begin{document}\input{intro}After.\end{document}".to_string()),
            ("intro.tex".to_string(), "Current text.\n\\endinput\n\\begin{conjecture}Discarded.\\end{conjecture}".to_string()),
        ];
        let flat = flatten(&files).unwrap();
        assert!(flat.contains("Current text.") && flat.contains("After.") && !flat.contains("Discarded"), "{flat}");
    }

    #[test]
    fn subfiles_are_not_the_root() {
        let doc = |body: &str| format!("\\documentclass{{article}}\\usepackage{{subfiles}}\\begin{{document}}{body}\\end{{document}}");
        let sub = |b: &str| format!("\\documentclass[../article.tex]{{subfiles}}\\begin{{document}}{b}\\end{{document}}");
        let files = vec![
            ("article.tex".to_string(), doc("\\subfile{sections/intro}\\subfile{sections/results}")),
            ("sections/intro.tex".to_string(), sub(&"Introduction text. ".repeat(40))),
            ("sections/results.tex".to_string(), sub("\\begin{conjecture}Results claim.\\end{conjecture}")),
        ];
        let flat = flatten(&files).unwrap();
        assert!(flat.contains("Introduction text.") && flat.contains("Results claim."), "{flat}");
    }

    #[test]
    fn iftrue_else_branch_is_not_content() {
        let t = r"\begin{document}\iftrue\begin{conjecture}Current.\end{conjecture}\else\begin{conjecture}Discarded.\end{conjecture}\fi\end{document}";
        assert_eq!(items(t).iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["Current."]);
    }

    #[test]
    fn iffalse_branches_are_not_content() {
        let t = r"\begin{document}\iffalse\begin{conjecture}Discarded.\end{conjecture}\fi\begin{conjecture}Actual.\end{conjecture}\iffalse X \ifx a b Y \fi Z \else\begin{conjecture}Live else.\end{conjecture}\fi\end{document}";
        let it = items(t);
        assert_eq!(it.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["Actual.", "Live else."]);
    }

    #[test]
    fn includeonly_limits_includes() {
        let files = vec![
            ("main.tex".to_string(), r"\documentclass{article}\includeonly{body}\begin{document}\include{body}\include{draft}\input{extra}\end{document}".to_string()),
            ("body.tex".to_string(), r"\section{Real}".to_string()),
            ("draft.tex".to_string(), r"\section{Draft}".to_string()),
            ("extra.tex".to_string(), r"\section{Extra}".to_string()),
        ];
        let flat = flatten(&files).unwrap();
        assert!(flat.contains("Real") && flat.contains("Extra") && !flat.contains("Draft"), "{flat}");
    }

    #[test]
    fn iffalse_inside_theorem_bodies_and_commented_fi() {
        let t = r"\begin{document}\begin{conjecture}\iffalse False.\else True.\fi\end{conjecture}\end{document}";
        assert_eq!(items(t)[0].text, "True.");
        let t2 = "\\begin{document}\n\\iffalse\n% \\fi\n\\begin{conjecture}Hidden.\\end{conjecture}\n\\fi\n\\begin{conjecture}Visible.\\end{conjecture}\n\\end{document}";
        assert_eq!(items(t2).iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["Visible."]);
    }

    #[test]
    fn ltx_and_latex_sources_are_accepted() {
        for n in ["main.ltx", "paper.latex"] {
            let f = vec![(n.to_string(), r"\documentclass{article}\begin{document}\section{S}x\end{document}".to_string())];
            assert!(flatten(&f).is_some(), "{n}");
        }
    }

    #[test]
    fn starred_verb_keeps_following_math() {
        assert_eq!(clean(r"\verb*|a b|, let $x^{*}=0$."), "a b, let $x^{*}=0$.");
    }

    #[test]
    fn theorem_numbered_within_another_theorem_is_unnumbered() {
        let t = r"\newtheorem{thm}{Theorem}\newtheorem{conj}{Conjecture}[thm]\begin{document}\begin{thm}A.\end{thm}\begin{conj}B.\end{conj}\begin{thm}C.\end{thm}\begin{conj}D.\end{conj}\end{document}";
        let l: Vec<String> = items(t).into_iter().map(|i| i.label).collect();
        assert_eq!(l, ["Theorem 1", "Conjecture", "Theorem 2", "Conjecture"]);
        // a sectioning parent keeps its numbering
        let t2 = r"\newtheorem{conj}{Conjecture}[section]\begin{document}\section{A}\begin{conj}B.\end{conj}\end{document}";
        assert_eq!(items(t2)[0].label, "Conjecture 1.1");
    }

    #[test]
    fn declaretheorem_parent_theorem_is_unnumbered() {
        let t = r"\declaretheorem[name=Theorem]{thm}\declaretheorem[name=Conjecture,numberwithin=thm]{conj}\begin{document}\begin{thm}A.\end{thm}\begin{conj}B.\end{conj}\end{document}";
        let l: Vec<String> = items(t).into_iter().map(|i| i.label).collect();
        assert_eq!(l, ["Theorem 1", "Conjecture"]);
    }

    #[test]
    fn spaced_url_commands_keep_percent_encoding() {
        assert_eq!(clean("See \\url {https://example.org/a%20b} for data.\nNext."), "See https://example.org/a%20b for data. Next.");
    }

    #[test]
    fn percent_encoded_urls_survive_comment_stripping() {
        assert_eq!(clean(r"see \url{https://example.org/a%20b} now"), "see https://example.org/a%20b now");
        assert_eq!(clean("real % comment\nnext"), "real next");
    }

    #[test]
    fn url_text_is_literal() {
        assert_eq!(clean(r"see \url{http://www.mit.edu/~alexrem/Sequences.pdf}."), "see http://www.mit.edu/~alexrem/Sequences.pdf.");
    }

    #[test]
    fn verbatim_end_document_does_not_hide_the_bibliography() {
        let t = r"\begin{document}Use \verb|\end{document}| to finish.\begin{thebibliography}{9}\bibitem{a} Real ref.\end{thebibliography}\end{document}";
        assert_eq!(refs(t).len(), 1);
    }

    #[test]
    fn bibitems_become_refs() {
        let t = r"\begin{document}x\begin{thebibliography}{9}\bibitem{a} A. Author, \emph{Title}, arXiv:2301.01234.\bibitem[Bo(2020)]{b} B. Other, J. Math. 3 (2020), doi:10.1000/xyz123.\end{thebibliography}\end{document}";
        let r = refs(t);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].arxiv.as_deref(), Some("2301.01234"));
        assert_eq!(r[1].n, 2);
        assert_eq!(r[1].doi.as_deref(), Some("10.1000/xyz123"));
        assert!(refs(r"\begin{document}no bib\end{document}").is_empty());
        let two = refs(r"\begin{document}\begin{thebibliography}{9}\bibitem{a} First.\end{thebibliography}\appendix\begin{thebibliography}{9}\bibitem{b} Second.\end{thebibliography}\end{document}");
        assert_eq!(two.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["First.", "Second."]);
        let h = refs(r"\begin{document}\begin{thebibliography}{9}\bibitem{x}\href{https://doi.org/10.1103/PhysRevLett.116.061102}{Phys. Rev. Lett. 116, 061102 (2016)}.\end{thebibliography}\end{document}");
        assert_eq!(h[0].doi.as_deref(), Some("10.1103/PhysRevLett.116.061102"));
        let sp = refs("\\begin{document}\\begin{thebibliography}{9}\\bibitem{x}\n% \\href{https://doi.org/10.1000/old}{Old}\n\\href {https://doi.org/10.1000/new}{New}.\\end{thebibliography}\\end{document}\\begin{thebibliography}{9}\\bibitem{z} Ghost.\\end{thebibliography}");
        assert_eq!(sp.len(), 1);
        assert_eq!(sp[0].doi.as_deref(), Some("10.1000/new"));
        let esc = refs(r"\begin{document}\begin{thebibliography}{9}\bibitem{x}\href{https://doi.org/10.1007/978-3-030-58452-8\_2}{Chapter title}.\end{thebibliography}\end{document}");
        assert_eq!(esc[0].doi.as_deref(), Some("10.1007/978-3-030-58452-8_2"));
    }
}
