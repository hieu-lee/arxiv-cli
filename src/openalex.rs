//! OpenAlex request builders and `/works` response parser (pure; no networking).
//!
//! Fallback metadata source when the arXiv API is throttled. OpenAlex has no arXiv
//! categories, comments or MSC classes, so those `Paper` fields stay empty; in exchange
//! it provides institutions per author, which saves the HTML fetch used for `insts`.

use crate::model::Paper;
use std::borrow::Cow;

/// OpenAlex source id of arXiv ("arXiv (Cornell University)").
pub const ARXIV_SOURCE: &str = "S4306400194";

const WORKS: &str = "https://api.openalex.org/works";
/// Only the fields `parse` reads (OpenAlex `select` is top-level only, so `locations` stays big).
const SELECT: &str = "title,display_name,doi,publication_date,updated_date,authorships,\
abstract_inverted_index,primary_location,locations,biblio,is_authors_truncated";
const MAX_IDS: usize = 50;
const MAX_PER_PAGE: usize = 200;

/// Optional credentials; each is appended as a query parameter when present.
#[derive(Clone, Copy, Debug, Default)]
pub struct Auth<'a> {
    pub api_key: Option<&'a str>,
    pub mailto: Option<&'a str>,
}

/// Result ordering requested from OpenAlex (applied before pagination).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// Text relevance when there is text, otherwise newest first.
    Relevance,
    NewestFirst,
    OldestFirst,
}

#[derive(Clone, Copy, Debug)]
pub struct Query<'a> {
    pub text: Option<&'a str>,
    /// AND of groups; names inside one group are alternatives (OR).
    pub authors: &'a [Vec<String>],
    /// "YYYY-MM-DD", inclusive.
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub order: Order,
    /// Clamped to 1..=200.
    pub per_page: usize,
    /// 1-based.
    pub page: usize,
}

/// Percent-encode everything except RFC 3986 unreserved characters (and '/' when `keep_slash`).
fn pct(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Filter values may not contain ',' or '|' (filter separators); turn them into spaces.
fn filter_value(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if matches!(c, ',' | '|') { ' ' } else { c }).collect();
    pct(&cleaned.split_whitespace().collect::<Vec<_>>().join(" "), false)
}

fn push_auth(url: &mut String, auth: &Auth) {
    if let Some(k) = auth.api_key.filter(|s| !s.is_empty()) {
        url.push_str("&api_key=");
        url.push_str(&pct(k, false));
    }
    if let Some(m) = auth.mailto.filter(|s| !s.is_empty()) {
        url.push_str("&mailto=");
        url.push_str(&pct(m, false));
    }
}

/// URL fetching up to 50 works by versionless arXiv id ("2301.01234", "math/0309285") through the
/// arXiv DOI (`10.48550/arxiv.<id>`, matched case-insensitively; alternatives joined with an
/// encoded `|` = OR). Must not be called with an empty slice.
pub fn ids_url(ids: &[String], auth: &Auth) -> String {
    let dois: Vec<String> = ids
        .iter()
        .take(MAX_IDS)
        .map(|id| format!("10.48550/arxiv.{}", pct(&id.to_ascii_lowercase(), true)))
        .collect();
    let mut url = format!("{WORKS}?filter=doi:{}&select={SELECT}&per_page={MAX_IDS}", dois.join("%7C"));
    push_auth(&mut url, auth);
    url
}

/// Search URL restricted to works hosted on arXiv. Text matches title+abstract (plain `search=`
/// would match full text and return ~100x more noise); each author is an AND-ed name filter.
pub fn search_url(q: &Query, auth: &Auth) -> String {
    let mut filter = format!("locations.source.id:{ARXIV_SOURCE}");
    if let Some(d) = q.since.filter(|s| !s.is_empty()) {
        filter.push_str(&format!(",from_publication_date:{}", filter_value(d)));
    }
    if let Some(d) = q.until.filter(|s| !s.is_empty()) {
        filter.push_str(&format!(",to_publication_date:{}", filter_value(d)));
    }
    let text = q.text.map(filter_value).filter(|s| !s.is_empty());
    if let Some(t) = &text {
        filter.push_str(&format!(",title_and_abstract.search:{t}"));
    }
    for group in q.authors {
        let names: Vec<String> = group.iter().map(|a| filter_value(a)).filter(|a| !a.is_empty()).collect();
        if !names.is_empty() {
            filter.push_str(&format!(",raw_author_name.search:{}", names.join("%7C")));
        }
    }
    let sort = match (q.order, text.is_some()) {
        (Order::OldestFirst, _) => "publication_date:asc",
        (Order::NewestFirst, _) | (Order::Relevance, false) => "publication_date:desc",
        (Order::Relevance, true) => "relevance_score:desc",
    };
    let mut url = format!(
        "{WORKS}?filter={filter}&sort={sort}&select={SELECT}&per_page={}&page={}",
        q.per_page.clamp(1, MAX_PER_PAGE),
        q.page.max(1)
    );
    push_auth(&mut url, auth);
    url
}

/// Minimal borrowed JSON tree. serde_json's `Value` costs ~10x more on real pages because every
/// `locations` entry (dozens per work, mostly irrelevant) would be allocated; this parser builds
/// only the keys `to_paper` reads and skips everything else without allocating.
#[derive(Debug)]
enum Val<'a> {
    Null,
    Bool(bool),
    Uint(u64),
    Str(Cow<'a, str>),
    Arr(Vec<Val<'a>>),
    Obj(Vec<(Cow<'a, str>, Val<'a>)>),
    /// `abstract_inverted_index` flattened to `(position, word)` pairs, in file order.
    Words(Vec<(u64, Cow<'a, str>)>),
}

/// Keys `to_paper`/`parse` read; every other key is skipped without allocating.
fn wanted(k: &str) -> bool {
    matches!(
        k,
        "meta" | "count" | "results" | "error" | "message" | "title" | "display_name" | "doi"
            | "publication_date" | "updated_date" | "authorships" | "author" | "raw_author_name"
            | "institutions" | "abstract_inverted_index" | "primary_location" | "locations"
            | "landing_page_url" | "pdf_url" | "source" | "type" | "biblio" | "volume" | "issue"
            | "first_page" | "last_page" | "version" | "is_authors_truncated" | "raw_affiliation_strings"
    )
}
const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    src: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn parse(src: &'a str) -> Result<Val<'a>, String> {
        let mut p = Parser { src, i: 0 };
        let v = p.value(0, true)?;
        p.ws();
        if p.i < src.len() {
            return Err(p.err("trailing characters"));
        }
        Ok(v)
    }

    fn err(&self, what: &str) -> String {
        format!("openalex: invalid JSON: {what} at byte {}", self.i)
    }

    fn ws(&mut self) {
        let b = self.src.as_bytes();
        while matches!(b.get(self.i), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.src.as_bytes().get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", c as char)))
        }
    }

    /// `pick`: build only whitelisted keys; otherwise just validate and skip.
    fn value(&mut self, depth: usize, pick: bool) -> Result<Val<'a>, String> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        self.ws();
        let b = self.src.as_bytes();
        match b.get(self.i).copied() {
            None => Err(self.err("unexpected end")),
            Some(b'"') => {
                let s = self.string(pick)?;
                Ok(s.map_or(Val::Null, Val::Str))
            }
            Some(b'{') => {
                self.i += 1;
                let mut fields = Vec::new();
                self.ws();
                if b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Val::Obj(fields));
                }
                loop {
                    self.ws();
                    if b.get(self.i) != Some(&b'"') {
                        return Err(self.err("expected object key"));
                    }
                    let key = self.string(pick)?;
                    self.eat(b':')?;
                    match key {
                        Some(k) if wanted(&k) => {
                            let v = if &*k == "abstract_inverted_index" {
                                self.word_index(depth + 1)?
                            } else {
                                self.value(depth + 1, true)?
                            };
                            fields.push((k, v));
                        }
                        _ => {
                            self.value(depth + 1, false)?;
                        }
                    }
                    self.ws();
                    match b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Val::Obj(fields));
                        }
                        _ => return Err(self.err("expected ',' or '}'")),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Val::Arr(items));
                }
                loop {
                    let v = self.value(depth + 1, pick)?;
                    if pick {
                        items.push(v);
                    }
                    self.ws();
                    match b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Val::Arr(items));
                        }
                        _ => return Err(self.err("expected ',' or ']'")),
                    }
                }
            }
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(c) if c == b'-' || c.is_ascii_digit() => {
                let start = self.i;
                while matches!(b.get(self.i), Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')) {
                    self.i += 1;
                }
                let t = &self.src[start..self.i];
                if t == "-" {
                    return Err(self.err("bad number"));
                }
                Ok(t.parse::<u64>().map_or(Val::Null, Val::Uint))
            }
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    /// `{"word": [positions], ..}` without a tree per word; anything else is skipped as `Null`.
    fn word_index(&mut self, depth: usize) -> Result<Val<'a>, String> {
        self.ws();
        if self.src.as_bytes().get(self.i) != Some(&b'{') {
            return self.value(depth, false);
        }
        self.i += 1;
        let mut words = Vec::new();
        self.ws();
        if self.src.as_bytes().get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Val::Words(words));
        }
        loop {
            self.ws();
            if self.src.as_bytes().get(self.i) != Some(&b'"') {
                return Err(self.err("expected object key"));
            }
            let word = self.string(true)?.unwrap_or_default();
            self.eat(b':')?;
            if let Val::Arr(positions) = self.value(depth + 1, true)? {
                words.extend(positions.into_iter().filter_map(|p| match p {
                    Val::Uint(p) => Some((p, word.clone())),
                    _ => None,
                }));
            }
            self.ws();
            match self.src.as_bytes().get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Val::Words(words));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn literal(&mut self, word: &str) -> Result<Val<'a>, String> {
        if self.src[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(match word {
                "true" => Val::Bool(true),
                "false" => Val::Bool(false),
                _ => Val::Null,
            })
        } else {
            Err(self.err("bad literal"))
        }
    }

    /// Parse a string starting at the opening quote; `None` when `build` is false.
    fn string(&mut self, build: bool) -> Result<Option<Cow<'a, str>>, String> {
        let b = self.src.as_bytes();
        let start = self.i + 1;
        let mut i = start;
        let mut escaped = false;
        loop {
            i = next_quote_or_backslash(b, i);
            match b.get(i) {
                None => {
                    self.i = i;
                    return Err(self.err("unterminated string"));
                }
                Some(b'"') => break,
                _ => {
                    escaped = true;
                    i += 2;
                }
            }
        }
        let raw = self.src.get(start..i).ok_or_else(|| self.err("bad string escape"))?;
        self.i = i + 1;
        if !build {
            return Ok(None);
        }
        if !escaped {
            return Ok(Some(Cow::Borrowed(raw)));
        }
        unescape(raw).map(|s| Some(Cow::Owned(s))).ok_or_else(|| self.err("bad string escape"))
    }
}

/// Index of the next `"` or `\\` at or after `i` (or `b.len()`); 8 bytes at a time.
fn next_quote_or_backslash(b: &[u8], mut i: usize) -> usize {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let has_zero = |v: u64| v.wrapping_sub(LO) & !v & HI != 0;
    while let Some(chunk) = b.get(i..i + 8) {
        let w = u64::from_le_bytes(chunk.try_into().unwrap());
        if has_zero(w ^ (LO * b'"' as u64)) || has_zero(w ^ (LO * b'\\' as u64)) {
            break;
        }
        i += 8;
    }
    while i < b.len() && b[i] != b'"' && b[i] != b'\\' {
        i += 1;
    }
    i
}

fn unescape(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars();
    let hex4 = |it: &mut std::str::Chars| -> Option<u32> {
        let mut n = 0;
        for _ in 0..4 {
            n = n * 16 + it.next()?.to_digit(16)?;
        }
        Some(n)
    };
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hi = hex4(&mut it)?;
                let cp = if (0xD800..0xDC00).contains(&hi) {
                    let mut look = it.clone();
                    match (look.next(), look.next()) {
                        (Some('\\'), Some('u')) => match hex4(&mut look) {
                            Some(lo) if (0xDC00..0xE000).contains(&lo) => {
                                it = look;
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            }
                            _ => 0xFFFD,
                        },
                        _ => 0xFFFD,
                    }
                } else {
                    hi
                };
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
            }
            _ => return None,
        }
    }
    Some(out)
}

impl<'a> Val<'a> {
    fn get(&self, key: &str) -> Option<&Val<'a>> {
        match self {
            Val::Obj(f) => f.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }
    fn items(&self) -> &[Val<'a>] {
        match self {
            Val::Arr(a) => a,
            _ => &[],
        }
    }
}

/// Trimmed, non-empty string field.
fn str_of<'v>(v: &'v Val, key: &str) -> Option<&'v str> {
    v.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Byte offset right after the first ASCII-case-insensitive occurrence of `needle` (lowercase).
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.len() > h.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n)).map(|i| i + n.len())
}

/// Validate and normalise an arXiv id at the start of `s`: drops version, ".pdf", query/fragment
/// and the old-style subject class ("math.GT/0309285" -> "math/0309285").
fn arxiv_id_at(s: &str) -> Option<String> {
    let end = s.find(['?', '#']).unwrap_or(s.len());
    let s = s[..end].trim_end_matches('/');
    let s = s.strip_suffix(".pdf").unwrap_or(s);
    let digits = |t: &[u8]| !t.is_empty() && t.iter().all(u8::is_ascii_digit);
    let unversioned = |t: &str| -> String {
        match t.rfind('v') {
            Some(i) if digits(&t.as_bytes()[i + 1..]) => t[..i].to_string(),
            _ => t.to_string(),
        }
    };
    if s.as_bytes().first()?.is_ascii_digit() {
        let id = unversioned(s);
        let (yymm, num) = id.split_once('.')?;
        return (yymm.len() == 4 && digits(yymm.as_bytes()) && matches!(num.len(), 4 | 5) && digits(num.as_bytes()))
            .then_some(id);
    }
    let (arch, num) = s.split_once('/')?;
    let arch = arch.split('.').next().unwrap_or("").to_ascii_lowercase();
    let num = unversioned(num);
    let arch_ok = !arch.is_empty() && arch.bytes().all(|c| c.is_ascii_lowercase() || c == b'-');
    (arch_ok && num.len() == 7 && digits(num.as_bytes())).then(|| format!("{arch}/{num}"))
}

/// arXiv id from a landing page / pdf URL ("arxiv.org/abs/..", "arxiv.org/pdf/..") or arXiv DOI.
fn arxiv_id_from_url(u: &str) -> Option<String> {
    for marker in ["arxiv.org/abs/", "arxiv.org/pdf/", "10.48550/arxiv."] {
        if let Some(i) = find_ci(u, marker) {
            return arxiv_id_at(&u[i..]);
        }
    }
    None
}

/// Every arXiv id a work carries: its top-level DOI, the primary location and all other locations.
fn arxiv_ids(w: &Val) -> Vec<String> {
    let from_loc = |l: &Val| ["landing_page_url", "pdf_url"].into_iter().filter_map(|k| str_of(l, k).and_then(arxiv_id_from_url)).collect::<Vec<_>>();
    let mut out: Vec<String> = Vec::new();
    let locs = w.get("locations").into_iter().flat_map(Val::items).flat_map(from_loc);
    let primary = w.get("primary_location").into_iter().flat_map(from_loc);
    for id in locs.chain(primary).chain(str_of(w, "doi").and_then(arxiv_id_from_url)) {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Rebuild the abstract from the `(position, word)` pairs. Positions that cannot occur in a
/// well-formed index (>= number of entries) are dropped, so junk cannot distort the text.
fn abstract_text(idx: &Val) -> String {
    let Val::Words(words) = idx else { return String::new() };
    let mut ordered: Vec<&(u64, Cow<str>)> = words.iter().filter(|(p, _)| *p < words.len() as u64).collect();
    ordered.sort_unstable_by_key(|(p, _)| *p);
    let mut out = String::with_capacity(ordered.iter().map(|(_, w)| w.len() + 1).sum());
    for (_, w) in ordered {
        out.push_str(w);
        out.push(' ');
    }
    squash(&out)
}

fn date_part(s: &str) -> Option<&str> {
    let d = s.get(..10)?;
    let b = d.as_bytes();
    (b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit()))
        .then_some(d)
}

fn bare_doi(s: &str) -> Option<String> {
    let d = &s[find_ci(s, "doi.org/").unwrap_or(0)..];
    (d.starts_with("10.") && !d.to_ascii_lowercase().starts_with("10.48550/")).then(|| d.to_string())
}

fn source_type<'v>(loc: &'v Val) -> Option<&'v str> {
    loc.get("source")?.get("type")?.as_str()
}

/// The published journal version: the primary location when it is a journal, otherwise a journal
/// location whose landing page is a DOI link (OpenAlex also lists bogus "journal" repositories).
fn journal_location<'v>(w: &'v Val) -> Option<&'v Val<'v>> {
    let is_journal = |l: &Val| source_type(l) == Some("journal");
    w.get("primary_location").filter(|l| is_journal(l)).or_else(|| {
        w.get("locations")?
            .items()
            .iter()
            .find(|l| is_journal(l) && str_of(l, "landing_page_url").and_then(bare_doi).is_some())
    })
}

fn journal_ref(w: &Val, loc: &Val, year: &str) -> Option<String> {
    let name = squash(loc.get("source")?.get("display_name")?.as_str()?);
    if name.is_empty() {
        return None;
    }
    let bib = |k: &str| w.get("biblio").and_then(|b| str_of(b, k));
    let (vol, issue, first, last) = (bib("volume"), bib("issue"), bib("first_page"), bib("last_page"));
    if vol.is_none() && issue.is_none() && first.is_none() {
        return Some(name);
    }
    let mut out = name;
    if let Some(v) = vol {
        out.push(' ');
        out.push_str(v);
    }
    if let Some(i) = issue {
        out.push_str(if vol.is_some() { "(" } else { " (" });
        out.push_str(i);
        out.push(')');
    }
    if let Some(f) = first {
        out.push_str(", ");
        out.push_str(f);
        match last {
            Some(l) if l != f => {
                out.push('-');
                out.push_str(l);
            }
            _ => {}
        }
    }
    if !year.is_empty() {
        out.push_str(&format!(" ({year})"));
    }
    Some(out)
}

/// "2024-03-01T00:00:00Z" for new-style ids "2403.01234" and old-style "math/0309285" (-> 2003-09).
fn id_month(id: &str) -> Option<String> {
    let digits = match id.split_once('/') {
        Some((_, n)) => n.get(..4)?,
        None => id.split_once('.')?.0,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (yy, mm): (u32, u32) = (digits[..2].parse().ok()?, digits[2..].parse().ok()?);
    if !(1..=12).contains(&mm) {
        return None;
    }
    let year = if yy >= 91 { 1900 + yy } else { 2000 + yy };
    Some(format!("{year:04}-{mm:02}-01T00:00:00Z"))
}

fn to_paper(w: &Val, id: String) -> Option<Paper> {
    let mut p = Paper { id, ..Paper::default() };
    p.title = squash(str_of(w, "title").or_else(|| str_of(w, "display_name")).unwrap_or(""));
    for a in w.get("authorships").into_iter().flat_map(Val::items) {
        let name = a.get("author").and_then(|x| str_of(x, "display_name")).or_else(|| str_of(a, "raw_author_name"));
        if let Some(n) = name {
            p.authors.push(squash(n));
        }
        for i in a.get("institutions").into_iter().flat_map(Val::items) {
            if let Some(n) = str_of(i, "display_name") {
                if !p.insts.iter().any(|x| x == n) {
                    p.insts.push(n.to_string());
                }
            }
        }
        // the author's own affiliation strings ("Google Brain") often name what a filter asks for even
        // when the canonical institution differs ("Google (United States)")
        for r in a.get("raw_affiliation_strings").into_iter().flat_map(Val::items) {
            if let Some(n) = r.as_str().map(squash).filter(|n| !n.is_empty()) {
                if !p.insts.iter().any(|x| *x == n) {
                    p.insts.push(n);
                }
            }
        }
    }
    // list responses carry only the first 100 authorships: then the author count and institutions are incomplete
    // (the flag may be absent from a projection, and OpenAlex caps the list at 100: a full-size list is suspect)
    let truncated = matches!(w.get("is_authors_truncated"), Some(Val::Bool(true))) || w.get("authorships").map_or(0, |a| a.items().len()) >= 100;
    p.partial = 1 | (u8::from(truncated) << 1);
    // institutions count as read only when OpenAlex has some and the list is complete; otherwise the arXiv
    // page is still consulted
    // (and every listed author must have an affiliation: a missing one may be what a filter asks for)
    let all_affiliated = w.get("authorships").is_some_and(|a| a.items().iter().all(|x| ["institutions", "raw_affiliation_strings"].iter().any(|k| x.get(k).is_some_and(|i| !i.items().is_empty()))));
    p.enriched = u8::from(!p.insts.is_empty() && !truncated && all_affiliated);
    p.abs = w.get("abstract_inverted_index").map(abstract_text).unwrap_or_default();
    // OpenAlex dates describe the published version / the record, not arXiv v1 / the latest version:
    // the arXiv month is encoded in the id itself, so use that (day precision is unknown).
    let stamp = |d: Option<&str>| d.and_then(date_part).map(|d| format!("{d}T00:00:00Z"));
    p.published = id_month(&p.id).or_else(|| stamp(str_of(w, "publication_date"))).unwrap_or_default();
    p.updated = p.published.clone();
    if let Some(loc) = journal_location(w) {
        p.doi = str_of(loc, "landing_page_url").and_then(bare_doi).or_else(|| str_of(w, "doi").and_then(bare_doi));
        // the journal year is the OpenAlex publication year (the paper's own year() is the arXiv month)
        let pub_year = str_of(w, "publication_date").and_then(date_part).map(|d| d[..4].to_string()).unwrap_or_else(|| p.year().to_string());
        p.jref = journal_ref(w, loc, &pub_year);
    } else {
        // proceedings and other published versions: a non-arXiv DOI still marks the paper as published
        // (only a location flagged as the published version: preprint-server or mirror DOIs do not count)
        let published: Vec<&Val> = w
            .get("locations")
            .into_iter()
            .flat_map(Val::items)
            .filter(|l| str_of(l, "version") == Some("publishedVersion"))
            .collect();
        let doi_of = |l: &Val| str_of(l, "landing_page_url").and_then(bare_doi).filter(|d| !d.to_ascii_lowercase().starts_with("10.48550/"));
        p.doi = published.iter().find_map(|l| doi_of(l));
        // a published conference version (even without a DOI, e.g. OpenReview) keeps its venue name so
        // --jn / --q can classify it
        if let Some(loc) = published.iter().find(|l| source_type(l) == Some("conference")) {
            let name = str_of(loc, "raw_source_name").or_else(|| loc.get("source").and_then(|s| str_of(s, "display_name"))).map(squash).filter(|n| !n.is_empty());
            let year = str_of(w, "publication_date").and_then(date_part).map(|d| d[..4].to_string());
            p.jref = name.map(|n| year.map_or(n.clone(), |y| format!("{n} ({y})")));
        }
    }
    Some(p)
}

/// Parse a `/works` response into `(papers, total_count)`. Works without a derivable arXiv id are
/// skipped; a missing `results` yields an empty list; an OpenAlex error body (`{"error": ..}`
/// without `results`) and invalid JSON are errors. `wanted` (by-id lookups): a work is returned once under each
/// requested id it carries (DOI or any location), so a merged work answers every alias that was asked for.
pub fn parse(json: &str, wanted: &[String]) -> Result<(Vec<Paper>, usize), String> {
    parse_response(json, wanted, usize::MAX).map(|p| (p.papers, p.total))
}

/// One response page: the arXiv works found, how many raw rows OpenAlex returned (works we could not
/// map to an arXiv id still count for paging), and the server-side total.
pub struct Page {
    pub papers: Vec<Paper>,
    pub rows: usize,
    pub total: usize,
}

/// `limit`: only the first `limit` rows of the response are used (a scan budget).
pub fn parse_page(json: &str, limit: usize) -> Result<Page, String> {
    parse_response(json, &[], limit)
}

fn parse_response(json: &str, wanted: &[String], limit: usize) -> Result<Page, String> {
    let v = Parser::parse(json)?;
    let Some(results) = v.get("results").filter(|r| matches!(r, Val::Arr(_))) else {
        return match (str_of(&v, "error"), str_of(&v, "message")) {
            (Some(e), m) => Err(format!("openalex: {e}{}", m.map(|m| format!(": {m}")).unwrap_or_default())),
            (None, Some(m)) => Err(format!("openalex: {m}")),
            _ => Ok(Page { papers: Vec::new(), rows: 0, total: 0 }),
        };
    };
    let items = &results.items()[..results.items().len().min(limit)];
    let rows = items.len();
    let papers: Vec<Paper> = items
        .iter()
        .flat_map(|w| {
            let ids = arxiv_ids(w);
            let hit: Vec<String> = wanted.iter().filter(|x| ids.contains(x)).cloned().collect();
            let ids = if hit.is_empty() { ids.into_iter().take(1).collect() } else { hit };
            ids.into_iter().filter_map(|id| to_paper(w, id))
        })
        .collect();
    let total = match v.get("meta").and_then(|m| m.get("count")) {
        Some(Val::Uint(n)) => *n as usize,
        _ => papers.len(),
    };
    Ok(Page { papers, rows, total })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const IDS: &str = include_str!("../tests/fixtures/openalex_ids.json");
    const SEARCH: &str = include_str!("../tests/fixtures/openalex_search.json");

    const NOAUTH: Auth = Auth { api_key: None, mailto: None };

    fn one(w: Value) -> Option<Paper> {
        let src = w.to_string();
        let w = Parser::parse(&src).unwrap();
        to_paper(&w, arxiv_ids(&w).into_iter().next()?)
    }

    fn abs(idx: Value) -> String {
        let src = json!({ "abstract_inverted_index": idx }).to_string();
        abstract_text(Parser::parse(&src).unwrap().get("abstract_inverted_index").unwrap())
    }

    /// `jref` of a work whose primary location is the journal "Physical Review Letters".
    fn jref(biblio: Value, year: &str) -> Option<String> {
        let w = json!({"publication_date": format!("{year}-01-01"), "biblio": biblio,
            "primary_location": {"landing_page_url": "https://doi.org/10.1103/x", "source": {"display_name": "Physical Review Letters", "type": "journal"}},
            "locations": [{"landing_page_url": "http://arxiv.org/abs/2301.00001"}]});
        one(w).unwrap().jref
    }

    #[test]
    fn ids_url_shape() {
        let ids = vec!["2301.01234".to_string(), "math/0309285".to_string(), "hep-th/9711200".to_string()];
        let u = ids_url(&ids, &NOAUTH);
        assert_eq!(
            u,
            format!(
                "https://api.openalex.org/works?filter=doi:10.48550/arxiv.2301.01234%7C10.48550/arxiv.math/0309285%7C10.48550/arxiv.hep-th/9711200&select={SELECT}&per_page=50"
            )
        );
        assert!(!u.contains('|') && !u.contains(' '));
    }

    #[test]
    fn ids_url_caps_at_50_and_lowercases() {
        let ids: Vec<String> = (0..80).map(|i| format!("2301.{:05}", i)).collect();
        let u = ids_url(&ids, &NOAUTH);
        assert_eq!(u.matches("10.48550").count(), 50);
        assert!(u.contains("10.48550/arxiv.2301.00049") && !u.contains("2301.00050"));
        let u = ids_url(&["Math/0309285".to_string()], &NOAUTH);
        assert!(u.contains("filter=doi:10.48550/arxiv.math/0309285&"));
    }

    #[test]
    fn auth_params_appended_and_encoded() {
        let ids = ["2301.01234".to_string()];
        let u = ids_url(&ids, &Auth { api_key: Some("k e&y"), mailto: Some("a+b@x.org") });
        assert!(u.ends_with("&per_page=50&api_key=k%20e%26y&mailto=a%2Bb%40x.org"), "{u}");
        let u = ids_url(&ids, &Auth { api_key: None, mailto: Some("") });
        assert!(u.ends_with("&per_page=50"));
    }

    #[test]
    fn search_url_full() {
        let authors = vec![vec!["Ashish Vaswani".to_string()], vec!["Shazeer".to_string(), "Parmar".to_string()]];
        let q = Query {
            text: Some("sparse attention"),
            authors: &authors,
            since: Some("2024-01-01"),
            until: Some("2024-12-31"),
            order: Order::Relevance,
            per_page: 25,
            page: 2,
        };
        assert_eq!(
            search_url(&q, &Auth { api_key: Some("KEY"), mailto: None }),
            format!(
                "https://api.openalex.org/works?filter=locations.source.id:S4306400194,from_publication_date:2024-01-01,to_publication_date:2024-12-31,title_and_abstract.search:sparse%20attention,raw_author_name.search:Ashish%20Vaswani,raw_author_name.search:Shazeer%7CParmar&sort=relevance_score:desc&select={SELECT}&per_page=25&page=2&api_key=KEY"
            )
        );
    }

    #[test]
    fn search_url_sort_and_clamps() {
        let base = Query { text: None, authors: &[], since: None, until: None, order: Order::Relevance, per_page: 0, page: 0 };
        let u = search_url(&base, &NOAUTH);
        // no text: relevance degrades to newest first
        assert!(u.contains("filter=locations.source.id:S4306400194&sort=publication_date:desc&"));
        assert!(u.ends_with("&per_page=1&page=1"));
        let u = search_url(&Query { order: Order::NewestFirst, text: Some("x"), per_page: 999, ..base }, &NOAUTH);
        assert!(u.contains("&sort=publication_date:desc&") && u.ends_with("&per_page=200&page=1"));
        // oldest first is honoured server-side even with text (never relevance first, then re-sorted)
        let u = search_url(&Query { order: Order::OldestFirst, text: Some("x"), ..base }, &NOAUTH);
        assert!(u.contains("&sort=publication_date:asc&"), "{u}");
    }

    #[test]
    fn search_url_sanitises_filter_separators() {
        let authors = vec![vec!["O'Brien, Pat|Q".to_string()], vec!["  ".to_string()]];
        let q = Query {
            text: Some("a,b | c/d: é"),
            authors: &authors,
            since: None,
            until: None,
            order: Order::Relevance,
            per_page: 10,
            page: 1,
        };
        let u = search_url(&q, &NOAUTH);
        assert!(u.contains(",title_and_abstract.search:a%20b%20c%2Fd%3A%20%C3%A9,"), "{u}");
        assert!(u.contains(",raw_author_name.search:O%27Brien%20Pat%20Q&sort="), "{u}");
        assert_eq!(u.matches("raw_author_name").count(), 1);
    }

    #[test]
    fn id_month_for_both_id_styles() {
        assert_eq!(id_month("2403.01234").as_deref(), Some("2024-03-01T00:00:00Z"));
        assert_eq!(id_month("math/0309285").as_deref(), Some("2003-09-01T00:00:00Z"));
        assert_eq!(id_month("hep-th/9711200").as_deref(), Some("1997-11-01T00:00:00Z"));
        assert_eq!(id_month("9999.1"), None);
        assert_eq!(id_month("2413.00001"), None);
        assert_eq!(id_month("junk"), None);
    }

    #[test]
    fn page_limit_caps_the_rows_read() {
        let pg = parse_page(SEARCH, 3).unwrap();
        assert_eq!((pg.rows, pg.papers.len()), (3, 3));
        assert_eq!(pg.total, 21007, "the server-side total is unaffected");
    }

    #[test]
    fn published_conference_without_doi_keeps_its_venue() {
        let w = json!({"publication_date": "2024-05-01", "locations": [
            {"landing_page_url": "http://arxiv.org/abs/2401.00001"},
            {"landing_page_url": "https://openreview.net/forum?id=abc", "version": "publishedVersion", "source": {"type": "conference", "display_name": "ICLR"}}]});
        let p = one(w).unwrap();
        assert_eq!((p.jref.as_deref(), p.doi.as_deref()), (Some("ICLR (2024)"), None));
    }

    #[test]
    fn id_lookup_keys_a_work_by_its_arxiv_doi() {
        let w = r#"{"results":[{"doi":"https://doi.org/10.48550/arxiv.2001.00001","locations":[{"landing_page_url":"http://arxiv.org/abs/2002.00002"}]}],"meta":{"count":1}}"#;
        let want = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parse(w, &want(&["2001.00001"])).unwrap().0[0].id, "2001.00001");
        // a merged work answers every alias that was asked for, and only those
        let ids = |v: &[&str]| parse(w, &want(v)).unwrap().0.into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(ids(&["2002.00002"]), ["2002.00002"]);
        assert_eq!(ids(&["2001.00001", "2002.00002"]), ["2001.00001", "2002.00002"]);
        assert_eq!(parse_page(w, usize::MAX).unwrap().papers[0].id, "2002.00002");
    }

    #[test]
    fn published_conference_doi_counts_but_preprint_mirror_dois_do_not() {
        let w = json!({"locations": [
            {"landing_page_url": "http://arxiv.org/abs/2401.00001"},
            {"landing_page_url": "https://doi.org/10.65215/abc", "version": "acceptedVersion"},
            {"landing_page_url": "https://doi.org/10.52202/079017-0532", "version": "publishedVersion", "source": {"type": "conference", "display_name": "NeurIPS"}}]});
        let p = one(w).unwrap();
        assert_eq!((p.jref.as_deref(), p.doi.as_deref()), (Some("NeurIPS"), Some("10.52202/079017-0532")));
        let w = json!({"locations": [{"landing_page_url": "http://arxiv.org/abs/2401.00001"},
            {"landing_page_url": "https://doi.org/10.65215/abc", "version": "acceptedVersion"}]});
        assert_eq!(one(w).unwrap().doi, None);
    }

    #[test]
    fn id_from_every_location_shape() {
        let id = |u: &str| arxiv_id_from_url(u);
        assert_eq!(id("http://arxiv.org/abs/2301.01234"), Some("2301.01234".into()));
        assert_eq!(id("https://arxiv.org/pdf/1706.03762v5"), Some("1706.03762".into()));
        assert_eq!(id("https://arxiv.org/pdf/1706.03762v5.pdf"), Some("1706.03762".into()));
        assert_eq!(id("https://arxiv.org/abs/2301.01234v2?context=cs"), Some("2301.01234".into()));
        assert_eq!(id("https://doi.org/10.48550/arxiv.1602.03837"), Some("1602.03837".into()));
        assert_eq!(id("https://doi.org/10.48550/arXiv.1602.03837"), Some("1602.03837".into()));
        assert_eq!(id("http://arxiv.org/abs/math/0309285"), Some("math/0309285".into()));
        assert_eq!(id("https://doi.org/10.48550/arxiv.math/0309285"), Some("math/0309285".into()));
        assert_eq!(id("https://arxiv.org/abs/hep-th/9711200v3"), Some("hep-th/9711200".into()));
        assert_eq!(id("https://arxiv.org/abs/math.GT/0309285"), Some("math/0309285".into()));
        assert_eq!(id("https://arxiv.org/abs/1501.00001"), Some("1501.00001".into()));
        assert_eq!(id("https://doi.org/10.1103/physrevlett.116.061102"), None);
        assert_eq!(id("https://arxiv.org/abs/"), None);
        assert_eq!(id("https://arxiv.org/abs/23011.0123456"), None);
        assert_eq!(id("https://arxiv.org/list/cs.AI/recent"), None);
    }

    #[test]
    fn id_prefers_locations_then_doi() {
        let w = json!({"doi": "https://doi.org/10.48550/arxiv.2001.00001",
            "locations": [{"landing_page_url": "https://x.org/1", "pdf_url": "https://arxiv.org/pdf/2002.00002v1"}]});
        assert_eq!(one(w).unwrap().id, "2002.00002");
        let w = json!({"doi": "https://doi.org/10.48550/arxiv.2001.00001", "locations": null});
        assert_eq!(one(w).unwrap().id, "2001.00001");
        let w = json!({"primary_location": {"landing_page_url": "http://arxiv.org/abs/math/0309285"}});
        assert_eq!(one(w).unwrap().id, "math/0309285");
        assert!(one(json!({"doi": "https://doi.org/10.1103/x", "locations": [{"landing_page_url": null}]})).is_none());
    }

    #[test]
    fn abstract_from_inverted_index_with_repeats() {
        let idx = json!({"the": [0, 4], "cat": [1], "sat": [2], "on": [3], "mat": [5], "the.": [6]});
        assert_eq!(abs(idx), "the cat sat on the mat the.");
        assert_eq!(abs(json!({"a": [0, 2, 4], "b": [1, 3]})), "a b a b a");
        assert_eq!(abs(json!({})), "");
        assert_eq!(abs(json!(null)), "");
    }

    #[test]
    fn abstract_junk_positions_are_bounded() {
        // out-of-range, negative, fractional and non-array entries are dropped; gaps collapse
        let idx = json!({"a": [0, 4000000000u64], "b": [-1, 1.5, 2], "c": "x", "d": [null]});
        assert_eq!(abs(idx), "a b");
    }

    #[test]
    fn journal_ref_formats() {
        let b = |v: Value, i: Value, f: Value, l: Value| json!({"volume": v, "issue": i, "first_page": f, "last_page": l});
        let (n, s) = (Value::Null, |x: &str| json!(x));
        assert_eq!(jref(b(s("116"), s("6"), s("061102"), s("061102")), "2016").unwrap(), "Physical Review Letters 116(6), 061102 (2016)");
        assert_eq!(jref(b(s("12"), n.clone(), s("105"), s("108")), "2005").unwrap(), "Physical Review Letters 12, 105-108 (2005)");
        assert_eq!(jref(b(n.clone(), s("3"), n.clone(), n.clone()), "2005").unwrap(), "Physical Review Letters (3) (2005)");
        assert_eq!(jref(b(s("9"), n.clone(), s("55"), n.clone()), "2005").unwrap(), "Physical Review Letters 9, 55 (2005)");
        assert_eq!(jref(b(n.clone(), n.clone(), n.clone(), n.clone()), "2016").unwrap(), "Physical Review Letters");
        assert_eq!(jref(json!({}), "2016").unwrap(), "Physical Review Letters");
        assert_eq!(jref(Value::Null, "2016").unwrap(), "Physical Review Letters");
    }

    #[test]
    fn fixture_ids_journal_and_old_style() {
        let (ps, total) = parse(IDS, &[]).unwrap();
        assert_eq!((ps.len(), total), (3, 3));
        let ids: Vec<&str> = ps.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["1602.03837", "math/0309285", "1706.03762"]);

        let ligo = &ps[0];
        assert_eq!(ligo.title, "Observation of Gravitational Waves from a Binary Black Hole Merger");
        assert_eq!(ligo.authors.len(), 12);
        assert_eq!(&ligo.authors[..2], ["B. P. Abbott", "R. Abbott"]);
        assert_eq!(ligo.insts[0], "California Institute of Technology");
        assert_eq!(ligo.insts.iter().filter(|i| *i == "California Institute of Technology").count(), 1);
        assert_eq!(ligo.jref.as_deref(), Some("Physical Review Letters 116(6), 061102 (2016)"));
        assert_eq!(ligo.doi.as_deref(), Some("10.1103/physrevlett.116.061102"));
        // dates come from the id (arXiv month), not from the journal publication date
        assert_eq!(ligo.published, "2016-02-01T00:00:00Z");
        assert_eq!(ligo.updated, ligo.published);
        assert_eq!((ligo.ver, ligo.enriched), (0, 1));
        assert!(ligo.cats.is_empty() && ligo.comment.is_none() && ligo.msc.is_empty() && ligo.venue.is_none());
        assert!(ligo.abs.starts_with("On September 14, 2015 at 09:50:45 UTC the two detectors of the Laser Interferometer"));

        let seg = &ps[1];
        assert_eq!(seg.title, "An algorithm for optimal partitioning of data on an interval");
        assert!(seg.abs.starts_with("Many signal processing problems can be solved by maximizing the fitness"));
        assert_eq!(seg.jref.as_deref(), Some("IEEE Signal Processing Letters 12(2), 105-108 (2005)"));
        assert_eq!(seg.doi.as_deref(), Some("10.1109/lsp.2001.838216"));
        assert_eq!(seg.url(), "https://arxiv.org/abs/math/0309285");
    }

    #[test]
    fn fixture_unpublished_work_with_foreign_doi_has_no_jref() {
        let (ps, _) = parse(IDS, &[]).unwrap();
        let att = &ps[2];
        assert_eq!(att.title, "Attention Is All You Need");
        assert_eq!((att.jref.clone(), att.doi.clone()), (None, None));
        assert!(!att.insts.is_empty() && att.authors.len() == 8);
    }

    #[test]
    fn fixture_search_page() {
        let (ps, total) = parse(SEARCH, &[]).unwrap();
        assert_eq!((ps.len(), total), (8, 21007));
        assert_eq!(ps[0].id, "2408.07092");
        assert_eq!(ps[0].published, "2024-08-01T00:00:00Z");
        assert_eq!(ps[0].insts[0], "Shanghai Jiao Tong University");
        assert_eq!(ps[0].jref, None);
        assert_eq!(ps[5].id, "2406.02542"); // arXiv location listed after a conference one
        // a published conference version keeps its venue name so --jn / --q can classify it
        assert_eq!(ps[5].jref.as_deref(), Some("neural information processing systems (2024)"));
        assert!(ps[5].doi.is_some());
        assert!(crate::venue::resolve(ps[5].jref.as_deref().unwrap()).is_some_and(|v| v.q >= 1), "venue table knows NeurIPS");
        assert_eq!(ps[7].id, "2312.17649");
        assert_eq!(ps[7].abs, ""); // abstract_inverted_index is null
        // institutions count as read only for works that have some; everything is flagged partial (no categories)
        assert!(ps.iter().all(|p| p.ver == 0 && p.partial & 1 == 1 && !p.title.is_empty()));
        assert!(ps.iter().any(|p| p.insts.is_empty()), "fixture has a work without institutions (2406.02542)");
        // incomplete affiliation data is never marked as read (2410.10165 lacks one author's institutions)
        assert!(ps.iter().all(|p| p.enriched == 0 || !p.insts.is_empty()));
        let partial = ps.iter().find(|p| p.id == "2410.10165").unwrap();
        assert!(!partial.insts.is_empty() && partial.enriched == 0);
    }

    #[test]
    fn select_requests_the_truncation_flag_and_full_size_lists_are_suspect() {
        assert!(SELECT.contains("is_authors_truncated"));
        let people: Vec<Value> = (0..100).map(|i| json!({"author": {"display_name": format!("P{i}")}, "institutions": [{"display_name": "MIT"}]})).collect();
        let p = one(json!({"authorships": people, "locations": [{"landing_page_url": "http://arxiv.org/abs/2401.00002"}]})).unwrap();
        assert_eq!((p.authors.len(), p.partial & 2, p.enriched), (100, 2, 0));
        let few: Vec<Value> = (0..99).map(|i| json!({"author": {"display_name": format!("P{i}")}, "institutions": [{"display_name": "MIT"}]})).collect();
        let p = one(json!({"authorships": few, "locations": [{"landing_page_url": "http://arxiv.org/abs/2401.00002"}]})).unwrap();
        assert_eq!((p.partial & 2, p.enriched), (0, 1));
    }

    #[test]
    fn raw_affiliation_strings_are_kept_next_to_canonical_institutions() {
        let w = json!({"authorships": [{"author": {"display_name": "A"},
            "institutions": [{"display_name": "Google (United States)"}],
            "raw_affiliation_strings": ["Google Brain", "Google (United States)"]}],
            "locations": [{"landing_page_url": "http://arxiv.org/abs/2401.00004"}]});
        let p = one(w).unwrap();
        assert_eq!(p.insts, ["Google (United States)", "Google Brain"]);
        assert_eq!(p.enriched, 1);
        // raw strings alone also count as affiliation data
        let w = json!({"authorships": [{"author": {"display_name": "A"}, "institutions": [], "raw_affiliation_strings": ["MIT, Cambridge"]}],
            "locations": [{"landing_page_url": "http://arxiv.org/abs/2401.00004"}]});
        assert_eq!(one(w).unwrap().enriched, 1);
    }

    #[test]
    fn one_unaffiliated_author_keeps_institutions_pending() {
        let w = json!({"authorships": [
            {"author": {"display_name": "A"}, "institutions": [{"display_name": "MIT"}]},
            {"author": {"display_name": "B"}, "institutions": []}],
            "locations": [{"landing_page_url": "http://arxiv.org/abs/2401.00003"}]});
        let p = one(w).unwrap();
        assert_eq!((p.insts.len(), p.enriched), (1, 0));
        let w = json!({"authorships": [{"author": {"display_name": "A"}, "institutions": [{"display_name": "MIT"}]}],
            "locations": [{"landing_page_url": "http://arxiv.org/abs/2401.00003"}]});
        assert_eq!(one(w).unwrap().enriched, 1);
    }

    #[test]
    fn truncated_author_lists_are_flagged_and_not_trusted() {
        let w = json!({"is_authors_truncated": true,
            "authorships": [{"author": {"display_name": "A"}, "institutions": [{"display_name": "MIT"}]}],
            "locations": [{"landing_page_url": "http://arxiv.org/abs/2401.00001"}]});
        let p = one(w).unwrap();
        assert_eq!((p.partial, p.enriched), (3, 0));
        assert_eq!(p.insts, ["MIT"]);
    }

    #[test]
    fn paging_counts_raw_rows_not_mapped_papers() {
        let js = r#"{"meta":{"count":500},"results":[{"doi":"https://doi.org/10.1/x"},{"doi":"https://doi.org/10.48550/arxiv.2301.00001"}]}"#;
        let pg = parse_page(js, usize::MAX).unwrap();
        assert_eq!((pg.papers.len(), pg.rows, pg.total), (1, 2, 500));
    }

    #[test]
    fn bogus_journal_locations_are_ignored() {
        let w = json!({
            "publication_date": "2020-05-01",
            "biblio": {"volume": "1", "issue": "2", "first_page": "3", "last_page": "4"},
            "primary_location": {"landing_page_url": "http://arxiv.org/abs/2005.00001", "source": {"type": "repository"}},
            "locations": [
                {"landing_page_url": "https://eprints.gla.ac.uk/view/x.html", "source": {"type": "journal", "display_name": "Civil War Book Review"}},
                {"landing_page_url": "https://doi.org/10.48550/arxiv.2005.00001", "source": {"type": "journal", "display_name": "arXiv"}}
            ]});
        let p = one(w).unwrap();
        assert_eq!((p.jref, p.doi), (None, None));
    }

    #[test]
    fn journal_location_fallback_when_primary_is_not_journal() {
        let w = json!({
            "publication_date": "2020-05-01",
            "biblio": {"volume": "7", "issue": null, "first_page": "10", "last_page": null},
            "primary_location": {"landing_page_url": "http://arxiv.org/abs/2005.00001", "source": {"type": "repository"}},
            "locations": [{"landing_page_url": "https://doi.org/10.1000/J.Abc", "source": {"type": "journal", "display_name": "J. Abc"}}]});
        let p = one(w).unwrap();
        assert_eq!(p.jref.as_deref(), Some("J. Abc 7, 10 (2020)"));
        assert_eq!(p.doi.as_deref(), Some("10.1000/J.Abc"));
    }

    #[test]
    fn whitespace_dates_and_null_fields() {
        let w = json!({
            "title": "  A \n  title\twith   gaps ",
            "publication_date": "2021-03-04", "updated_date": "2025-01-02T03:04:05.123456",
            "authorships": [
                {"author": {"display_name": " Ada  Lovelace "}, "institutions": [{"display_name": "X Univ"}, {"display_name": null}]},
                {"author": null, "raw_author_name": "Raw Name", "institutions": [{"display_name": "X Univ"}, {"display_name": "Y Lab"}]},
                {"author": {"display_name": ""}, "institutions": null}, 7, null],
            "locations": [{"landing_page_url": "http://arxiv.org/abs/2103.00001"}]});
        let p = one(w).unwrap();
        assert_eq!(p.title, "A title with gaps");
        assert_eq!(p.authors, ["Ada Lovelace", "Raw Name"]);
        assert_eq!(p.insts, ["X Univ", "Y Lab"]);
        assert_eq!((p.published.as_str(), p.updated.as_str()), ("2021-03-01T00:00:00Z", "2021-03-01T00:00:00Z"));
        let p = one(json!({"title": null, "display_name": "Fallback", "publication_date": "bad", "updated_date": 5,
            "locations": [{"pdf_url": "https://arxiv.org/pdf/2103.00001"}]}))
        .unwrap();
        assert_eq!((p.title.as_str(), p.published.as_str(), p.updated.as_str()), ("Fallback", "2021-03-01T00:00:00Z", "2021-03-01T00:00:00Z"));
        let p = one(json!({"publication_date": "2021-03-04", "locations": [{"pdf_url": "https://arxiv.org/pdf/2103.00001"}]})).unwrap();
        assert_eq!(p.updated, "2021-03-01T00:00:00Z"); // id month wins over the journal date
    }

    #[test]
    fn skips_non_arxiv_works_and_counts() {
        let j = r#"{"meta":{"count":42},"results":[
            {"title":"no arxiv","locations":[{"landing_page_url":"https://example.org/a"}]},
            {"title":"yes","locations":[{"landing_page_url":"http://arxiv.org/abs/2301.00001"}]}]}"#;
        let (ps, total) = parse(j, &[]).unwrap();
        assert_eq!((ps.len(), total, ps[0].title.as_str()), (1, 42, "yes"));
        // no meta.count: falls back to the number of parsed papers
        let (ps, total) = parse(r#"{"results":[{"doi":"https://doi.org/10.48550/arxiv.2301.00001"}]}"#, &[]).unwrap();
        assert_eq!((ps.len(), total), (1, 1));
    }

    #[test]
    fn junk_and_empty_input() {
        assert_eq!(parse(r#"{"meta":{"count":0},"results":[]}"#, &[]).unwrap().1, 0);
        for ok in ["{}", "[]", "null", "42", r#""str""#, r#"{"results":null}"#, r#"{"results":{"a":1}}"#, r#"{"results":[1,null,"x",[],{}]}"#] {
            let (ps, total) = parse(ok, &[]).unwrap();
            assert_eq!((ps.len(), total), (0, 0), "{ok}");
        }
        for bad in ["", "not json", "{", r#"{"results":[{"title":"x""#, "<html>502</html>"] {
            assert!(parse(bad, &[]).is_err(), "{bad}");
        }
        assert!(parse(&IDS[..IDS.len() / 2], &[]).is_err());
        let e = parse(r#"{"error":"Invalid query parameters","message":"bad filter"}"#, &[]).unwrap_err();
        assert_eq!(e, "openalex: Invalid query parameters: bad filter");
        assert_eq!(parse(r#"{"message":"Rate limited"}"#, &[]).unwrap_err(), "openalex: Rate limited");
    }

    #[test]
    fn multibyte_dates_do_not_panic() {
        assert_eq!(date_part("2021-0é-04"), None);
        assert_eq!(date_part("é"), None);
        assert_eq!(date_part("2021-03-04T10:00"), Some("2021-03-04"));
        assert_eq!(arxiv_id_from_url("https://arxiv.org/abs/é2301.01234"), None);
        assert_eq!(arxiv_id_from_url("arxiv.org/abs/日本語/1234567"), None);
    }

    #[test]
    #[ignore]
    fn bench_parse_200_works() {
        let page: Value = serde_json::from_str(SEARCH).unwrap();
        let many: Vec<&Value> = page["results"].as_array().unwrap().iter().cycle().take(200).collect();
        let big = json!({"meta": {"count": 200}, "results": many}).to_string();
        let mut best = std::time::Duration::MAX;
        for _ in 0..50 {
            let t = std::time::Instant::now();
            let (ps, _) = parse(&big, &[]).unwrap();
            best = best.min(t.elapsed());
            assert_eq!(ps.len(), 200);
        }
        let t = std::time::Instant::now();
        let _: Value = serde_json::from_str(&big).unwrap();
        eprintln!("parse of 200 works ({} KB): best {best:?}; serde_json::Value alone: {:?}", big.len() / 1024, t.elapsed());
    }
}
