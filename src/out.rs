//! Output renderers, designed for minimum tokens for an LLM reader.
//!
//! Default `l` format: one paper per line, `|`-separated, no header:
//! `id|YYYY-MM|A. Au, B. Bu +3|cat|Title`. Columns that are empty for every paper are dropped;
//! otherwise empty cells keep their `|` so columns stay aligned. The abstract (`ab`) is not a
//! column: it goes on its own continuation line indented by one space.
//! Literal `|` inside a value is written `\|`.

use crate::args::Args;
use crate::model::*;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    L,
    Tsv,
    Json,
    Jsonl,
    Ids,
    Bib,
    Md,
}

const FORMATS: &str = "l,tsv,json,jsonl,ids,bib,md";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Id,
    Ver,
    T,
    A,
    Aff,
    Inst,
    D,
    Dd,
    U,
    C,
    Pc,
    Ab,
    Co,
    Jr,
    Doi,
    Pub,
    Q,
    V,
    Msc,
    Url,
    Pdf,
}

const FIELD_TABLE: &[(&str, Field)] = &[
    ("id", Field::Id),
    ("ver", Field::Ver),
    ("t", Field::T),
    ("a", Field::A),
    ("aff", Field::Aff),
    ("inst", Field::Inst),
    ("d", Field::D),
    ("dd", Field::Dd),
    ("u", Field::U),
    ("c", Field::C),
    ("pc", Field::Pc),
    ("ab", Field::Ab),
    ("co", Field::Co),
    ("jr", Field::Jr),
    ("doi", Field::Doi),
    ("pub", Field::Pub),
    ("q", Field::Q),
    ("v", Field::V),
    ("msc", Field::Msc),
    ("url", Field::Url),
    ("pdf", Field::Pdf),
];

const FIELD_LIST: &str = "id,ver,t,a,aff,inst,d,dd,u,c,pc,ab,co,jr,doi,pub,q,v,msc,url,pdf";

impl Field {
    pub fn parse(code: &str) -> Option<Field> {
        FIELD_TABLE.iter().find(|(c, _)| *c == code).map(|(_, f)| *f)
    }
    pub fn code(self) -> &'static str {
        FIELD_TABLE.iter().find(|(_, f)| *f == self).map(|(c, _)| *c).unwrap_or("?")
    }
}

#[derive(Clone, Debug)]
pub struct Fmt {
    format: Format,
    fields: Vec<Field>,
    abslen: usize,
    aumax: usize,
    tok: Option<usize>,
    full: bool,
}

fn num_flag(a: &Args, name: &str) -> Result<Option<usize>, String> {
    match a.get(name) {
        None => Ok(None),
        Some(v) => v.trim().parse::<usize>().map(Some).map_err(|_| format!("--{name} needs a non-negative integer, got {v:?}")),
    }
}

impl Fmt {
    /// Reads --fmt --fields --abslen --aumax --tok --full. `default_fields` used when --fields absent.
    pub fn from_args(a: &Args, default_fields: &str) -> Result<Fmt, String> {
        let format = match a.get("fmt").map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("l") => Format::L,
            Some("tsv") => Format::Tsv,
            Some("json") => Format::Json,
            Some("jsonl") => Format::Jsonl,
            Some("ids") => Format::Ids,
            Some("bib") => Format::Bib,
            Some("md") => Format::Md,
            Some(x) => return Err(format!("unknown --fmt {x:?}; valid: {FORMATS}")),
        };
        let mut fields = Vec::new();
        for code in a.get("fields").unwrap_or(default_fields).split(',') {
            let code = code.trim().to_ascii_lowercase();
            if code.is_empty() {
                continue;
            }
            let f = Field::parse(&code).ok_or_else(|| format!("unknown field {code:?}; valid: {FIELD_LIST}"))?;
            if !fields.contains(&f) {
                fields.push(f);
            }
        }
        if fields.is_empty() {
            return Err(format!("--fields is empty; valid: {FIELD_LIST}"));
        }
        let tok = num_flag(a, "tok")?;
        if tok == Some(0) {
            return Err("--tok must be > 0".into());
        }
        Ok(Fmt {
            format,
            fields,
            abslen: num_flag(a, "abslen")?.unwrap_or(280),
            aumax: num_flag(a, "aumax")?.unwrap_or(3),
            tok,
            full: a.flag("full"),
        })
    }
    pub fn full(&self) -> bool {
        self.full
    }
    /// Effective abstract length limit in chars (`usize::MAX` with --full).
    pub fn abslen(&self) -> usize {
        if self.full {
            usize::MAX
        } else {
            self.abslen
        }
    }
    /// Effective author count limit (`usize::MAX` with --full).
    pub fn aumax(&self) -> usize {
        if self.full {
            usize::MAX
        } else {
            self.aumax
        }
    }
}

// ---------------------------------------------------------------- text helpers

fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Truncate to at most `max` chars including the trailing `…`, at a word boundary when possible.
/// Never splits a char. Strings of <= `max` chars are returned unchanged.
pub fn trunc(s: &str, max: usize) -> String {
    let Some((cut, _)) = s.char_indices().nth(max) else {
        return s.to_string();
    };
    if max == 0 {
        return String::new();
    }
    let (idx, _) = s.char_indices().nth(max - 1).unwrap_or((cut, ' '));
    let head = &s[..idx];
    let word_end = if s[idx..].starts_with(char::is_whitespace) {
        head
    } else {
        match head.rfind(char::is_whitespace) {
            Some(i) if i > 0 => &head[..i],
            _ => head,
        }
    };
    let word_end = word_end.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-'));
    format!("{word_end}…")
}

/// JSON string literal (with quotes). Non-ASCII passes through; control chars are escaped.
pub fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Fast token estimate: ASCII chars / 3.6 (rounded up) + 1 per non-ASCII char.
/// Slightly pessimistic for English prose (~4 chars/token) so budgets are never overshot.
pub fn est_tokens(s: &str) -> usize {
    let (mut ascii, mut other) = (0usize, 0usize);
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    (ascii * 5 + 17) / 18 + other
}

/// Cut `s` to ~`tok` tokens at a line boundary and append a marker line saying how much was dropped.
/// The first line is always kept (lines are never cut), so one huge line may exceed `tok`.
pub fn clip(s: String, tok: usize) -> String {
    if est_tokens(&s) <= tok {
        return s;
    }
    let budget = tok.saturating_sub(6);
    let (mut used, mut end) = (0usize, 0usize);
    for line in s.split_inclusive('\n') {
        let c = est_tokens(line);
        if end > 0 && used + c > budget {
            break;
        }
        used += c;
        end += line.len();
    }
    if end >= s.len() {
        return s;
    }
    // only short header-like lines fit (id, title, section heading) and the next line is one huge body
    // line (raw text): keep a word-boundary prefix of it
    if used * 2 <= budget {
        let room = budget.saturating_sub(used) * 3;
        let rest = &s[end..];
        let line_len = rest.find('\n').unwrap_or(rest.len());
        if room > 0 && line_len > room {
            let mut cut = room;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            let cut = rest[..cut].rfind(' ').filter(|&p| p > room / 2).unwrap_or(cut);
            let dropped = est_tokens(&rest[cut..]);
            return format!("{}{}…\n…[-{dropped} tok]", &s[..end], &rest[..cut]);
        }
    }
    let dropped = est_tokens(&s[end..]);
    let mut out = s[..end].trim_end_matches('\n').to_string();
    out.push_str(&format!("\n…[-{dropped} tok]"));
    out
}

// ---------------------------------------------------------------- names

const SUFFIXES: &[&str] = &["jr", "sr", "ii", "iii", "iv"];
const CAP_PARTICLES: &[&str] = &["von", "der", "den", "de", "del", "della", "di", "da", "dos", "du", "ten", "ter"];
const COLLAB_WORDS: &[&str] = &["collaboration", "consortium", "team", "group", "committee", "project", "network"];

fn is_particle(tok: &str) -> bool {
    match tok.chars().next() {
        Some(c) if c.is_lowercase() || c == '\'' || c == '\u{2019}' => true,
        _ => CAP_PARTICLES.contains(&tok.to_lowercase().as_str()),
    }
}

struct Name<'a> {
    given: Vec<&'a str>,
    surname: String,
    suffix: Option<&'a str>,
}

/// "Smirnov A. Yu.": one full word followed only by dotted initials.
fn surname_first(toks: &[&str]) -> bool {
    toks.len() >= 2 && !is_initials(toks[0]) && !toks[0].ends_with(',') && toks[1..].iter().all(|t| is_initials(t))
}

/// Dotted initials: "A.", "Yu.", "A.V.", "J.-P." (every letter run at most two letters).
pub fn is_initials(t: &str) -> bool {
    t.ends_with('.')
        && t.chars().all(|c| c.is_alphabetic() || c == '.' || c == '-')
        && t.split(['.', '-']).all(|run| run.chars().count() <= 2)
        && t.chars().any(char::is_alphabetic)
}

fn parse_name(name: &str) -> Option<Name<'_>> {
    let mut toks: Vec<&str> = name.split_whitespace().collect();
    if toks.len() < 2 || toks.iter().any(|t| COLLAB_WORDS.contains(&t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase().as_str())) {
        return None;
    }
    let suffix = match toks.last() {
        Some(l) if toks.len() >= 3 && SUFFIXES.contains(&l.trim_end_matches('.').to_lowercase().as_str()) => toks.pop(),
        _ => None,
    };
    // "van der Waerden, Bartel Leendert": everything up to the comma is the surname
    // "Smirnov A. Yu.": a trailing run of initials means the name is written surname-first
    if surname_first(&toks) {
        return Some(Name { given: toks[1..].to_vec(), surname: toks[0].to_string(), suffix });
    }
    if let Some(ci) = toks.iter().position(|t| t.ends_with(',')).filter(|&ci| ci + 1 < toks.len()) {
        let sur = toks[..=ci].join(" ").trim_end_matches(',').to_string();
        return Some(Name { given: toks[ci + 1..].to_vec(), surname: sur, suffix });
    }
    let last = toks.len() - 1;
    let s = (1..last).find(|&i| is_particle(toks[i])).unwrap_or(last);
    Some(Name { given: toks[..s].to_vec(), surname: toks[s..].join(" "), suffix })
}

fn initial_of(tok: &str) -> String {
    // packed dotted initials ("Yu.A.") are separate initials ("Yu. A.")
    if tok.matches('.').count() >= 2 && !tok.contains('-') {
        let parts: Vec<String> = tok.split_inclusive('.').filter(|p| p.chars().any(char::is_alphabetic)).map(initial_of).collect();
        return parts.join(" ");
    }
    let parts: Vec<String> = tok
        .split('-')
        .map(|part| {
            let mut v = String::new();
            let letters = part.chars().filter(|c| c.is_alphabetic());
            let chars: Vec<char> = part.chars().collect();
            // a transliterated given-name initial ("Yu.", "Zh.") is one initial, not two
            let digraph = chars.len() == 3 && chars[2] == '.' && chars[0].is_uppercase() && chars[1].is_lowercase();
            if digraph {
                v.push_str(part);
            } else if part.contains('.') {
                for c in letters {
                    if !v.is_empty() {
                        v.push(' ');
                    }
                    v.push(c);
                    v.push('.');
                }
            } else if let Some(c) = letters.into_iter().next() {
                v.push(c);
                v.push('.');
            }
            v
        })
        .filter(|v| !v.is_empty())
        .collect();
    parts.join("-")
}

/// "Terence Tao" -> "T. Tao"; "Bartel Leendert van der Waerden" -> "B. L. van der Waerden";
/// "Jean-Pierre Serre" -> "J.-P. Serre"; "Tao, Terence" -> "T. Tao". Collaboration names and
/// single-token names are returned unchanged (whitespace-collapsed).
pub fn initials(name: &str) -> String {
    let Some(n) = parse_name(name) else {
        return collapse(name);
    };
    let mut out = String::with_capacity(name.len());
    for g in &n.given {
        let ini = initial_of(g);
        if !ini.is_empty() {
            out.push_str(&ini);
            out.push(' ');
        }
    }
    out.push_str(&n.surname);
    if let Some(s) = n.suffix {
        out.push(' ');
        out.push_str(s);
    }
    out
}

/// Initials form, first `max` authors, then ` +K` for the K omitted: "A. Au, B. Bu +3".
pub fn short_authors(authors: &[String], max: usize) -> String {
    let shown = authors.len().min(max);
    let mut out = String::new();
    for (i, a) in authors[..shown].iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&initials(a));
    }
    if authors.len() > shown {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("+{}", authors.len() - shown));
    }
    out
}

// ---------------------------------------------------------------- paper cells

/// Institution names only (drop departments/addresses), deduped, at most 3.
pub fn short_insts(v: &[String]) -> String {
    const KEYS: &[&str] = &["univ", "inst", "college", "school", "acad", "labor", "center", "centre", "polytech", "ecole", "école", "hochschule", "eth ", "mit", "research", "cnrs", "max planck", "inria", "cern"];
    let mut out: Vec<String> = Vec::new();
    for raw in v {
        let segs: Vec<&str> = raw.split([',', ';']).map(str::trim).filter(|x| !x.is_empty()).collect();
        let picked: Vec<&str> = segs.iter().copied().filter(|s| {
            let l = s.to_lowercase();
            KEYS.iter().any(|k| l.contains(k)) && !l.starts_with("department") && !l.starts_with("dept") && !l.starts_with("school of math") && !l.starts_with("faculty")
        }).take(1).collect();
        let name = match picked.first().or(segs.first()) {
            Some(n) => trunc(&collapse(n), 48),
            None => continue,
        };
        if !out.contains(&name) {
            out.push(name);
        }
        if out.len() == 3 {
            break;
        }
    }
    out.join("; ")
}

fn join_collapsed(v: &[String], sep: &str) -> String {
    collapse(&v.join(sep))
}

fn cell(p: &Paper, f: Field, o: &Fmt) -> String {
    match f {
        Field::Id => p.id.clone(),
        Field::Ver => {
            if p.ver > 0 {
                format!("v{}", p.ver)
            } else {
                String::new()
            }
        }
        Field::T => collapse(&p.title),
        Field::A => short_authors(&p.authors, o.aumax()),
        Field::Aff => join_collapsed(&p.affs, "; "),
        Field::Inst => {
            if o.full() {
                join_collapsed(&p.insts, "; ")
            } else {
                short_insts(&p.insts)
            }
        }
        Field::D => p.published.get(..7).unwrap_or(&p.published).to_string(),
        Field::Dd => p.date().to_string(),
        Field::U => p.updated_date().to_string(),
        Field::C => p.cats.join(" "),
        Field::Pc => p.primary().to_string(),
        Field::Ab => trunc(&collapse(&p.abs), o.abslen()),
        Field::Co => p.comment.as_deref().map(collapse).unwrap_or_default(),
        Field::Jr => p.jref.as_deref().map(collapse).unwrap_or_default(),
        Field::Doi => p.doi.clone().unwrap_or_default(),
        Field::Pub => (if p.is_published() { "P" } else { "-" }).to_string(),
        Field::Q => p.venue.map(|v| format!("Q{}", v.q)).unwrap_or_else(|| "-".into()),
        Field::V => p.venue.map(|v| v.name.to_string()).unwrap_or_default(),
        Field::Msc => p.msc.join(" "),
        Field::Url => p.url(),
        Field::Pdf => format!("https://arxiv.org/pdf/{}", p.id),
    }
}

fn esc_pipe(s: &str) -> String {
    s.replace('|', "\\|")
}

fn render_l(ps: &[Paper], f: &Fmt) -> (Vec<String>, Vec<String>) {
    let ab = f.fields.contains(&Field::Ab);
    let mut cols: Vec<Field> = f.fields.iter().copied().filter(|c| *c != Field::Ab).collect();
    if cols.is_empty() {
        cols.push(Field::Id);
    }
    let rows: Vec<Vec<String>> = ps.iter().map(|p| cols.iter().map(|c| esc_pipe(&cell(p, *c, f))).collect()).collect();
    let mut keep: Vec<bool> = (0..cols.len()).map(|j| rows.iter().any(|r| !r[j].is_empty())).collect();
    if !keep.iter().any(|k| *k) {
        keep[0] = true;
    }
    let chunks = ps
        .iter()
        .zip(&rows)
        .map(|(p, r)| {
            let mut line = r.iter().zip(&keep).filter(|(_, k)| **k).map(|(c, _)| c.as_str()).collect::<Vec<_>>().join("|");
            if ab {
                let a = cell(p, Field::Ab, f);
                if !a.is_empty() {
                    line.push_str("\n ");
                    line.push_str(&a);
                }
            }
            line
        })
        .collect();
    (Vec::new(), chunks)
}

fn render_tsv(ps: &[Paper], f: &Fmt) -> (Vec<String>, Vec<String>) {
    let head = f.fields.iter().map(|c| c.code()).collect::<Vec<_>>().join("\t");
    let chunks = ps.iter().map(|p| f.fields.iter().map(|c| cell(p, *c, f)).collect::<Vec<_>>().join("\t")).collect();
    (vec![head], chunks)
}

fn json_obj(p: &Paper, f: &Fmt) -> String {
    let mut o = String::from("{");
    for fl in &f.fields {
        let v = if *fl == Field::Ver {
            if p.ver == 0 {
                continue;
            }
            p.ver.to_string()
        } else {
            let c = cell(p, *fl, f);
            if c.is_empty() {
                continue;
            }
            json_str(&c)
        };
        if o.len() > 1 {
            o.push(',');
        }
        o.push_str(&json_str(fl.code()));
        o.push(':');
        o.push_str(&v);
    }
    o.push('}');
    o
}

fn render_md(p: &Paper, f: &Fmt) -> String {
    let mut s = format!("- [{}]({}) **{}**", p.id, p.url(), collapse(&p.title));
    if !p.authors.is_empty() {
        s.push_str(" — ");
        s.push_str(&short_authors(&p.authors, f.aumax()));
    }
    if let Some(d) = p.published.get(..7) {
        s.push_str(&format!(" ({d})"));
    }
    if f.fields.contains(&Field::Ab) && !p.abs.is_empty() {
        s.push_str("\n  ");
        s.push_str(&cell(p, Field::Ab, f));
    }
    s
}

/// Render paper list. `total` = server-reported total matches when known.
/// With a `--tok` budget trailing papers are dropped (whole papers only, at least one is kept)
/// and the footer reads `(showing K of N, budget)`.
pub fn papers(ps: &[Paper], total: Option<usize>, f: &Fmt) -> String {
    if ps.is_empty() {
        return match f.format {
            Format::Json => "[]".into(),
            Format::Jsonl | Format::Ids => String::new(),
            _ => "0 results".into(),
        };
    }
    let n = ps.len();
    let (head, chunks): (Vec<String>, Vec<String>) = match f.format {
        Format::L => render_l(ps, f),
        Format::Tsv => render_tsv(ps, f),
        Format::Json | Format::Jsonl => (Vec::new(), ps.iter().map(|p| json_obj(p, f)).collect()),
        Format::Ids => (Vec::new(), ps.iter().map(|p| p.id.clone()).collect()),
        Format::Bib => {
            // distinct papers must never share a citation key
            let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
            let entries = ps
                .iter()
                .map(|p| {
                    let base = bib_key(p);
                    let mut key = base.clone();
                    let mut k = 0u32;
                    while !taken.insert(key.clone()) {
                        key = format!("{base}{}", alpha_suffix(k));
                        k += 1;
                    }
                    bibtex_keyed(p, &key)
                })
                .collect();
            (Vec::new(), entries)
        }
        Format::Md => (Vec::new(), ps.iter().map(|p| render_md(p, f)).collect()),
    };
    let of = total.unwrap_or(n).max(n);
    let mut k = n;
    if let Some(budget) = f.tok {
        let mut used: usize = head.iter().map(|h| est_tokens(h) + 1).sum::<usize>() + 12;
        k = 0;
        for c in &chunks {
            used += est_tokens(c) + 1;
            if k > 0 && used > budget {
                break;
            }
            k += 1;
        }
    }
    let footer = if k < n {
        Some((k, of, true))
    } else if of > k {
        Some((k, of, false))
    } else {
        None
    };
    let shown = &chunks[..k];
    match f.format {
        Format::Json => {
            let mut s = format!("[{}", shown.join(","));
            if let Some((k, of, _)) = footer {
                s.push_str(&format!(",{{\"showing\":{k},\"of\":{of}}}"));
            }
            s.push(']');
            s
        }
        Format::Jsonl => {
            let mut s = shown.join("\n");
            if let Some((k, of, _)) = footer {
                s.push_str(&format!("\n{{\"showing\":{k},\"of\":{of}}}"));
            }
            s
        }
        fm => {
            let sep = if fm == Format::Bib { "\n\n" } else { "\n" };
            let mut s = head.join("\n");
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&shown.join(sep));
            // `ids` stays strictly one id per line (pipeline friendly): no footer
            if let (Some((k, of, budget)), false) = (footer, fm == Format::Ids) {
                s.push_str(&format!("{}(showing {k} of {of}{})", sep, if budget { ", budget" } else { "" }));
            }
            s
        }
    }
}

// ---------------------------------------------------------------- facets

const FACETS: &str = "cat,pcat,year,month,au,q,venue,pub,inst";
const FACET_TOP: usize = 15;

fn bump(m: &mut HashMap<String, usize>, k: &str) {
    if let Some(c) = m.get_mut(k) {
        *c += 1;
    } else {
        m.insert(k.to_string(), 1);
    }
}

pub fn facets(ps: &[Paper], by: &[String]) -> String {
    if ps.is_empty() {
        return "0 results".into();
    }
    let mut lines = Vec::new();
    for b in by {
        let mut m: HashMap<String, usize> = HashMap::new();
        match b.as_str() {
            "cat" => ps.iter().flat_map(|p| p.cats.iter()).for_each(|c| bump(&mut m, c)),
            "pcat" => ps.iter().filter(|p| !p.primary().is_empty()).for_each(|p| bump(&mut m, p.primary())),
            "year" => ps.iter().filter(|p| !p.year().is_empty()).for_each(|p| bump(&mut m, p.year())),
            "month" => ps.iter().filter_map(|p| p.published.get(..7)).for_each(|d| bump(&mut m, d)),
            "au" => {
                // count full names; show initials unless two distinct people would collide ("A. Smith")
                let mut full: HashMap<String, usize> = HashMap::new();
                ps.iter().flat_map(|p| p.authors.iter()).for_each(|a| bump(&mut full, &collapse(a)));
                let mut by_initials: HashMap<String, Vec<(String, usize)>> = HashMap::new();
                for (f, c) in full {
                    by_initials.entry(initials(&f)).or_default().push((f, c));
                }
                for (short, list) in by_initials {
                    if list.len() == 1 {
                        m.insert(short, list[0].1);
                    } else {
                        m.extend(list);
                    }
                }
            }
            "q" => ps.iter().for_each(|p| bump(&mut m, &p.venue.map(|v| format!("Q{}", v.q)).unwrap_or_else(|| "?".into()))),
            "venue" => ps.iter().filter_map(|p| p.venue).for_each(|v| bump(&mut m, v.name)),
            "pub" => ps.iter().for_each(|p| bump(&mut m, if p.is_published() { "P" } else { "-" })),
            "inst" => ps.iter().flat_map(|p| p.insts.iter()).for_each(|i| bump(&mut m, &collapse(i))),
            x => {
                lines.push(format!("unknown facet {x:?}; valid: {FACETS}"));
                continue;
            }
        }
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let rest = v.len().saturating_sub(FACET_TOP);
        let mut line = format!("{b}:");
        if v.is_empty() {
            line.push_str(" none");
        }
        for (i, (k, c)) in v.iter().take(FACET_TOP).enumerate() {
            line.push_str(if i == 0 { " " } else { ", " });
            line.push_str(&format!("{k} {c}"));
        }
        if rest > 0 {
            line.push_str(&format!(", +{rest}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

// ---------------------------------------------------------------- bibtex

fn fold_ascii(s: &str) -> String {
    const MAP: &[(&str, &str)] = &[
        ("àáâãäåāăąÀÁÂÃÄÅĀĂĄ", "a"),
        ("æÆ", "ae"),
        ("çćčĉċÇĆČĈĊ", "c"),
        ("ďđĎĐ", "d"),
        ("èéêëēĕėęěÈÉÊËĒĔĖĘĚ", "e"),
        ("ĝğġģĜĞĠĢ", "g"),
        ("ĥħĤĦ", "h"),
        ("ìíîïĩīĭįıÌÍÎÏĨĪĬĮİ", "i"),
        ("ĵĴ", "j"),
        ("ķĶ", "k"),
        ("ĺļľŀłĹĻĽĿŁ", "l"),
        ("ñńņňÑŃŅŇ", "n"),
        ("òóôõöøōŏőÒÓÔÕÖØŌŎŐ", "o"),
        ("œŒ", "oe"),
        ("ŕŗřŔŖŘ", "r"),
        ("śŝşšŚŜŞŠ", "s"),
        ("ß", "ss"),
        ("ţťŧŢŤŦ", "t"),
        ("þÞ", "th"),
        ("ùúûüũūŭůűųÙÚÛÜŨŪŬŮŰŲ", "u"),
        ("ŵŴ", "w"),
        ("ýÿŷÝŶŸ", "y"),
        ("źżžŹŻŽ", "z"),
    ];
    let mut o = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            o.push(c.to_ascii_lowercase());
        } else if let Some((_, r)) = MAP.iter().find(|(set, _)| set.contains(c)) {
            o.push_str(r);
        }
    }
    o
}

const TITLE_SKIP: &[&str] = &["a", "an", "the", "on", "of", "in", "for", "and", "to"];

fn bib_key(p: &Paper) -> String {
    let sur = p
        .authors
        .first()
        .map(|a| match parse_name(a) {
            Some(n) => fold_ascii(&n.surname),
            None => fold_ascii(a),
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "anon".into());
    let word = p
        .title
        .split_whitespace()
        .map(fold_ascii)
        .find(|w| !w.is_empty() && !TITLE_SKIP.contains(&w.as_str()))
        .unwrap_or_default();
    format!("{sur}{}{word}", p.year())
}

/// b, c, ..., z, bb, bc, ... (bijective base 25 over b..z): unbounded and unique per index.
fn alpha_suffix(k: u32) -> String {
    let mut n = k as u64 + 1;
    let mut s = Vec::new();
    while n > 0 {
        n -= 1;
        s.push(b'b' + (n % 25) as u8);
        n /= 25;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

fn bib_esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut prev = ' ';
    for c in s.chars() {
        if matches!(c, '%' | '&' | '#') && prev != '\\' {
            o.push('\\');
        }
        o.push(c);
        prev = c;
    }
    o
}

/// One BibTeX author: corporate/collaboration names are braced (one unit), suffixes use the
/// `Surname, Jr., Given` form so BibTeX does not mistake "Jr." for the surname.
fn bib_author(a: &str) -> String {
    let a = collapse(a);
    match parse_name(&a) {
        None => format!("{{{}}}", bib_esc(&a)),
        Some(n) => match n.suffix {
            Some(suf) => bib_esc(&format!("{}, {}, {}", n.surname.trim_end_matches(','), suf, n.given.join(" ").trim_end_matches(','))),
            // written surname-first ("Smirnov A. Yu."): BibTeX needs the comma form to find the surname
            None if surname_first(&a.split_whitespace().collect::<Vec<_>>()) => bib_esc(&format!("{}, {}", n.surname, n.given.join(" "))),
            None => bib_esc(&a),
        },
    }
}

fn bibtex_keyed(p: &Paper, key: &str) -> String {
    let kind = if p.jref.is_some() { "article" } else { "misc" };
    let mut s = format!("@{kind}{{{key},\ntitle={{{}}},", bib_esc(&collapse(&p.title)));
    if !p.authors.is_empty() {
        let names: Vec<String> = p.authors.iter().map(|a| bib_author(a)).collect();
        s.push_str(&format!("\nauthor={{{}}},", names.join(" and ")));
    }
    if !p.year().is_empty() {
        s.push_str(&format!("\nyear={{{}}},", p.year()));
    }
    if let Some(j) = &p.jref {
        s.push_str(&format!("\njournal={{{}}},", bib_esc(&collapse(j))));
    }
    if let Some(d) = &p.doi {
        s.push_str(&format!("\ndoi={{{d}}},"));
    }
    s.push_str(&format!("\neprint={{{}}},\narchivePrefix={{arXiv}},", p.id));
    if !p.primary().is_empty() {
        s.push_str(&format!("\nprimaryClass={{{}}},", p.primary()));
    }
    s.push_str(&format!("\nurl={{{}}}\n}}", p.url()));
    s
}

// ---------------------------------------------------------------- paper content

fn header(p: &Paper) -> String {
    format!("{} {}", p.id, trunc(&collapse(&p.title), 70))
}

/// Items found in one paper (header line with id/title, then items).
/// Item text is truncated to 3x `abslen` chars unless --full.
pub fn items(p: &Paper, it: &[Item], f: &Fmt) -> String {
    let mut s = header(p);
    if it.is_empty() {
        s.push_str("\n0 items");
        return s;
    }
    let max = f.abslen().saturating_mul(3);
    for i in it {
        s.push_str("\n[");
        s.push_str(i.kind.code());
        if !i.label.is_empty() {
            s.push(' ');
            s.push_str(&collapse(&i.label));
        }
        if !i.sec.is_empty() {
            s.push_str(" § ");
            s.push_str(&i.sec);
        }
        s.push_str("] ");
        s.push_str(&trunc(&collapse(&i.text), max));
    }
    s
}

/// Header line, then `1.2 Title` per section, one extra space of indent per level below the top one.
pub fn outline(p: &Paper, secs: &[Section]) -> String {
    let mut s = header(p);
    if secs.is_empty() {
        s.push_str("\n0 sections");
        return s;
    }
    let base = secs.iter().map(|x| x.level).min().unwrap_or(0);
    for x in secs {
        s.push('\n');
        for _ in 0..(x.level - base) {
            s.push(' ');
        }
        s.push_str(&x.id);
        s.push(' ');
        s.push_str(&collapse(&x.title));
    }
    s
}

pub fn hits(p: &Paper, h: &[Hit]) -> String {
    let mut s = header(p);
    if h.is_empty() {
        s.push_str("\n0 hits");
        return s;
    }
    for x in h {
        s.push('\n');
        if !x.sec.is_empty() {
            s.push('§');
            s.push_str(&x.sec);
            s.push(' ');
        }
        s.push_str(&format!("@{}: ", x.pos));
        let sn = collapse(&x.snippet);
        if !sn.starts_with('…') {
            s.push('…');
        }
        s.push_str(&sn);
        if !sn.ends_with('…') {
            s.push('…');
        }
    }
    s
}

/// `[n] text (arXiv:id) (doi:..)`; text truncated to 160 chars unless --full.
pub fn refs(p: &Paper, r: &[Ref], f: &Fmt) -> String {
    let max = if f.full() { usize::MAX } else { 160 };
    let mut s = header(p);
    if r.is_empty() {
        s.push_str("\n0 refs");
        return s;
    }
    for x in r {
        let text = collapse(&x.text);
        let shown = trunc(&text, max);
        s.push_str(&format!("\n[{}] {}", x.n, shown));
        if let Some(a) = x.arxiv.as_deref().filter(|a| !shown.contains(a)) {
            s.push_str(&format!(" (arXiv:{a})"));
        }
        if let Some(d) = x.doi.as_deref().filter(|d| !shown.contains(d)) {
            s.push_str(&format!(" (doi:{d})"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::parse;

    fn fmt(args: &[&str], def: &str) -> Fmt {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        Fmt::from_args(&parse(&v).unwrap(), def).unwrap()
    }
    fn au(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn paper(id: &str, title: &str, authors: &[&str], cats: &[&str], published: &str) -> Paper {
        Paper {
            id: id.into(),
            ver: 2,
            title: title.into(),
            authors: au(authors),
            cats: au(cats),
            published: published.into(),
            updated: "2024-05-06T07:08:09Z".into(),
            ..Default::default()
        }
    }
    fn p1() -> Paper {
        paper("2301.01234", "Sums of  two\ncubes", &["Terence Tao", "Ben Green", "Imre Ruzsa", "Tim Gowers", "Emmanuel Breuillard"], &["math.NT", "math.CO"], "2023-01-03T10:00:00Z")
    }
    fn p2() -> Paper {
        paper("math/0309285", "On a conjecture", &["Bartel Leendert van der Waerden"], &["math.GM"], "2003-09-16T10:00:00Z")
    }

    #[test]
    fn initials_basic() {
        assert_eq!(initials("Terence Tao"), "T. Tao");
        assert_eq!(initials("  Terence   Chi-Shen  Tao "), "T. C.-S. Tao");
        assert_eq!(initials("Euler"), "Euler");
        assert_eq!(initials("Ørjan Johansen"), "Ø. Johansen");
    }
    #[test]
    fn initials_particles_and_forms() {
        assert_eq!(initials("Bartel Leendert van der Waerden"), "B. L. van der Waerden");
        assert_eq!(initials("Jean de la Fontaine"), "J. de la Fontaine");
        assert_eq!(initials("Maria Da Silva"), "M. Da Silva");
        assert_eq!(initials("Jean-Pierre Serre"), "J.-P. Serre");
        assert_eq!(initials("J.H. Conway"), "J. H. Conway");
        assert_eq!(initials("John H. Conway"), "J. H. Conway");
        assert_eq!(initials("Tao, Terence"), "T. Tao");
        assert_eq!(initials("Martin Luther King Jr."), "M. L. King Jr.");
    }
    #[test]
    fn initials_collab_unchanged() {
        assert_eq!(initials("The ATLAS Collaboration"), "The ATLAS Collaboration");
        assert_eq!(initials(""), "");
    }
    #[test]
    fn bib_escapes_hash() {
        let p = paper("1005.2678", "The complexity of weighted and unweighted #CSP", &["A B"], &["cs.CC"], "2010-05-01T00:00:00Z");
        assert!(bibtex_keyed(&p, "k").contains("title={The complexity of weighted and unweighted \\#CSP},"));
    }

    #[test]
    fn bib_authors_brace_collaborations_and_move_suffixes() {
        let mut p = paper("2501.00001", "T", &["The ATLAS Collaboration", "Martin Luther King Jr."], &["hep-ex"], "2025-01-01T00:00:00Z");
        assert!(bibtex_keyed(&p, "k").contains("author={{The ATLAS Collaboration} and King, Jr., Martin Luther},"), "{}", bibtex_keyed(&p, "k"));
        p.authors = au(&["John Smith, Jr."]);
        assert!(bibtex_keyed(&p, "k").contains("author={Smith, Jr., John},"), "{}", bibtex_keyed(&p, "k"));
        p.authors = au(&["Terence Tao"]);
        assert!(bibtex_keyed(&p, "k").contains("author={Terence Tao},"));
        p.authors = au(&["Smith, John, Jr."]);
        assert!(bibtex_keyed(&p, "k").contains("author={Smith, Jr., John},"), "{}", bibtex_keyed(&p, "k"));
        p.authors = au(&["Smirnov A. Yu."]);
        assert!(bibtex_keyed(&p, "k").contains("author={Smirnov, A. Yu.},"), "{}", bibtex_keyed(&p, "k"));
    }

    #[test]
    fn apostrophe_particles_stay_with_the_surname() {
        assert_eq!(initials("Gerard 't Hooft"), "G. 't Hooft");
        assert_eq!(initials("Gerard \u{2019}t Hooft"), "G. \u{2019}t Hooft");
    }

    #[test]
    fn surname_first_with_trailing_initials() {
        assert_eq!(initials("Smirnov A. Yu."), "A. Yu. Smirnov");
        assert_eq!(initials("Terence Tao"), "T. Tao");
        assert_eq!(initials("A. Tao"), "A. Tao");
        assert_eq!(initials("J.-P. Serre"), "J.-P. Serre");
        assert_eq!(initials("Smirnov A.V."), "A. V. Smirnov");
        assert_eq!(initials("Serre J.-P."), "J.-P. Serre");
        assert_eq!(initials("Tao T."), "T. Tao");
        assert_eq!(initials("Yu.A. Simonov"), "Yu. A. Simonov");
    }

    #[test]
    fn comma_form_keeps_compound_surname() {
        assert_eq!(initials("van der Waerden, Bartel Leendert"), "B. L. van der Waerden");
        assert_eq!(initials("Tao, Terence"), "T. Tao");
    }

    #[test]
    fn short_authors_plus_k() {
        let a = au(&["Anna Au", "Bob Bu", "Cy Cu", "Di Du"]);
        assert_eq!(short_authors(&a, 3), "A. Au, B. Bu, C. Cu +1");
        assert_eq!(short_authors(&a, 4), "A. Au, B. Bu, C. Cu, D. Du");
        assert_eq!(short_authors(&a, usize::MAX), "A. Au, B. Bu, C. Cu, D. Du");
        assert_eq!(short_authors(&a, 0), "+4");
        assert_eq!(short_authors(&[], 3), "");
    }
    #[test]
    fn trunc_word_boundary() {
        assert_eq!(trunc("one two three four", 18), "one two three four");
        assert_eq!(trunc("one two three four", 13), "one two…");
        assert_eq!(trunc("one two three four", 14), "one two three…");
        assert_eq!(trunc("abcdefghij", 5), "abcd…");
        assert_eq!(trunc("one, two three", 9), "one, two…");
        assert_eq!(trunc("anything", 0), "");
    }
    #[test]
    fn trunc_unicode_char_boundary() {
        assert_eq!(trunc("ééééé ééééé", 6), "ééééé…");
        assert_eq!(trunc("日本語日本語日本語", 4), "日本語…");
        assert_eq!(trunc("日本語", 3), "日本語");
        assert_eq!(trunc("αβγ δεζ ηθι", 8), "αβγ δεζ…");
    }
    #[test]
    fn json_str_escapes() {
        assert_eq!(json_str("a\"b\\c\n\t\u{1}é"), "\"a\\\"b\\\\c\\n\\t\\u0001é\"");
    }
    #[test]
    fn est_tokens_values() {
        assert_eq!(est_tokens(""), 0);
        assert_eq!(est_tokens("abcd"), 2);
        assert_eq!(est_tokens(&"a".repeat(36)), 10);
        assert_eq!(est_tokens("日本語"), 3);
    }

    #[test]
    fn l_default_line() {
        let f = fmt(&[], "id,d,a,pc,t");
        assert_eq!(papers(&[p1()], None, &f), "2301.01234|2023-01|T. Tao, B. Green, I. Ruzsa +2|math.NT|Sums of two cubes");
    }
    #[test]
    fn l_two_rows_and_full_authors() {
        let f = fmt(&["--full"], "id,a,t");
        assert_eq!(
            papers(&[p1(), p2()], None, &f),
            "2301.01234|T. Tao, B. Green, I. Ruzsa, T. Gowers, E. Breuillard|Sums of two cubes\nmath/0309285|B. L. van der Waerden|On a conjecture"
        );
    }
    #[test]
    fn l_empty_column_dropped_or_aligned() {
        let f = fmt(&[], "id,jr,t");
        assert_eq!(papers(&[p2()], None, &f), "math/0309285|On a conjecture");
        let mut a = p1();
        a.jref = Some("Ann. Math. 1 (2020)".into());
        assert_eq!(papers(&[a, p2()], None, &f), "2301.01234|Ann. Math. 1 (2020)|Sums of two cubes\nmath/0309285||On a conjecture");
    }
    #[test]
    fn l_all_columns_empty_falls_back() {
        let f = fmt(&[], "co");
        assert_eq!(papers(&[p2()], None, &f), "");
        let f = fmt(&[], "ab");
        assert_eq!(papers(&[p2()], None, &f), "math/0309285");
    }
    #[test]
    fn l_pipe_escaped() {
        let mut a = p2();
        a.title = "Norms $|x|$ and a|b".into();
        let f = fmt(&[], "id,t");
        assert_eq!(papers(&[a], None, &f), "math/0309285|Norms $\\|x\\|$ and a\\|b");
    }
    #[test]
    fn l_abstract_continuation_and_trunc() {
        let mut a = p2();
        a.abs = "We  prove\nthat every integer is a sum of cubes of something big.".into();
        let f = fmt(&["--abslen", "30"], "id,ab");
        assert_eq!(papers(&[a.clone()], None, &f), "math/0309285\n We prove that every integer…");
        let f = fmt(&["--full"], "id,ab");
        assert_eq!(papers(&[a, p1()], None, &f), "math/0309285\n We prove that every integer is a sum of cubes of something big.\n2301.01234");
    }
    #[test]
    fn l_ver_field_and_dates() {
        let f = fmt(&[], "id,ver,d,dd,u");
        assert_eq!(papers(&[p1()], None, &f), "2301.01234|v2|2023-01|2023-01-03|2024-05-06");
        let mut a = p1();
        a.ver = 0;
        assert_eq!(papers(&[a], None, &fmt(&[], "id,ver,pub,q")), "2301.01234|-|-");
    }
    #[test]
    fn l_pub_q_v_fields() {
        let mut a = p1();
        a.doi = Some("10.1/x".into());
        a.venue = Some(Venue { name: "Annals of Math", q: 1, area: "math" });
        let f = fmt(&[], "id,pub,q,v,doi,c,msc,url,pdf");
        assert_eq!(
            papers(&[a], None, &f),
            "2301.01234|P|Q1|Annals of Math|10.1/x|math.NT math.CO|https://arxiv.org/abs/2301.01234|https://arxiv.org/pdf/2301.01234"
        );
    }
    #[test]
    fn tsv_header_and_sanitized() {
        let mut a = p2();
        a.title = "x\ty\nz".into();
        a.comment = Some("12\tpages".into());
        let f = fmt(&["--fmt", "tsv"], "id,t,co,jr");
        assert_eq!(papers(&[a], None, &f), "id\tt\tco\tjr\nmath/0309285\tx y z\t12 pages\t");
    }
    #[test]
    fn json_valid_and_typed() {
        let mut a = p1();
        a.title = "A \"quoted\" \\ title".into();
        let f = fmt(&["--fmt", "json"], "id,ver,t,doi");
        let s = papers(&[a, p2()], None, &f);
        assert_eq!(s, "[{\"id\":\"2301.01234\",\"ver\":2,\"t\":\"A \\\"quoted\\\" \\\\ title\"},{\"id\":\"math/0309285\",\"ver\":2,\"t\":\"On a conjecture\"}]");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["t"], "A \"quoted\" \\ title");
    }
    #[test]
    fn jsonl_lines() {
        let f = fmt(&["--fmt", "jsonl"], "id,pc");
        assert_eq!(papers(&[p1(), p2()], Some(5), &f), "{\"id\":\"2301.01234\",\"pc\":\"math.NT\"}\n{\"id\":\"math/0309285\",\"pc\":\"math.GM\"}\n{\"showing\":2,\"of\":5}");
    }
    #[test]
    fn ids_format() {
        let f = fmt(&["--fmt", "ids"], "id,t");
        assert_eq!(papers(&[p1(), p2()], None, &f), "2301.01234\nmath/0309285");
    }
    #[test]
    fn bib_misc_and_key_folding() {
        let mut a = p1();
        a.authors = au(&["Łukasz Müller-Ñandú", "B. Green"]);
        a.title = "The Étude of 50% sums & more".into();
        assert_eq!(
            bibtex_keyed(&a, &bib_key(&a)),
            "@misc{mullernandu2023etude,\ntitle={The Étude of 50\\% sums \\& more},\nauthor={Łukasz Müller-Ñandú and B. Green},\nyear={2023},\neprint={2301.01234},\narchivePrefix={arXiv},\nprimaryClass={math.NT},\nurl={https://arxiv.org/abs/2301.01234}\n}"
        );
    }
    #[test]
    fn bib_article_with_jref_and_particle_key() {
        let mut a = p2();
        a.jref = Some("Math. Ann.  1 (1920)".into());
        a.doi = Some("10.1/z".into());
        assert_eq!(
            bibtex_keyed(&a, &bib_key(&a)),
            "@article{vanderwaerden2003conjecture,\ntitle={On a conjecture},\nauthor={Bartel Leendert van der Waerden},\nyear={2003},\njournal={Math. Ann. 1 (1920)},\ndoi={10.1/z},\neprint={math/0309285},\narchivePrefix={arXiv},\nprimaryClass={math.GM},\nurl={https://arxiv.org/abs/math/0309285}\n}"
        );
    }
    #[test]
    fn bib_format_separates_entries() {
        let f = fmt(&["--fmt", "bib"], "id");
        let s = papers(&[p1(), p2()], None, &f);
        assert_eq!(s.matches("\n\n@").count(), 1);
        assert!(s.starts_with("@misc{tao2023sums,"));
    }
    #[test]
    fn md_format() {
        let f = fmt(&["--fmt", "md"], "id");
        assert_eq!(
            papers(&[p1(), p2()], None, &f),
            "- [2301.01234](https://arxiv.org/abs/2301.01234) **Sums of two cubes** — T. Tao, B. Green, I. Ruzsa +2 (2023-01)\n- [math/0309285](https://arxiv.org/abs/math/0309285) **On a conjecture** — B. L. van der Waerden (2003-09)"
        );
    }
    #[test]
    fn footer_rules() {
        let f = fmt(&[], "id");
        assert_eq!(papers(&[p1()], Some(40), &f), "2301.01234\n(showing 1 of 40)");
        assert_eq!(papers(&[p1()], Some(1), &f), "2301.01234");
        assert_eq!(papers(&[p1()], None, &f), "2301.01234");
        assert_eq!(papers(&[p1()], Some(0), &f), "2301.01234");
    }
    #[test]
    fn zero_results() {
        assert_eq!(papers(&[], Some(0), &fmt(&[], "id")), "0 results");
        assert_eq!(papers(&[], None, &fmt(&["--fmt", "json"], "id")), "[]");
        assert_eq!(facets(&[], &au(&["cat"])), "0 results");
    }
    #[test]
    fn budget_drops_trailing_papers() {
        let ps: Vec<Paper> = (0..10).map(|i| paper(&format!("2301.0000{i}"), "Title words here", &["A B"], &["math.CO"], "2023-01-03T10:00:00Z")).collect();
        let f = fmt(&["--tok", "40"], "id,t");
        assert_eq!(
            papers(&ps, Some(10), &f),
            "2301.00000|Title words here\n2301.00001|Title words here\n2301.00002|Title words here\n(showing 3 of 10, budget)"
        );
        let f = fmt(&["--tok", "1"], "id,t");
        assert_eq!(papers(&ps, None, &f), "2301.00000|Title words here\n(showing 1 of 10, budget)");
        let f = fmt(&["--tok", "500"], "id,t");
        assert!(!papers(&ps, Some(10), &f).contains("showing"));
    }
    #[test]
    fn budget_json_stays_valid() {
        let ps: Vec<Paper> = (0..6).map(|i| paper(&format!("2301.0000{i}"), "T", &[], &[], "2023")).collect();
        let f = fmt(&["--fmt", "json", "--tok", "30"], "id");
        let s = papers(&ps, Some(9), &f);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.last().unwrap()["of"], 9);
        assert!(arr.len() < 7);
    }
    #[test]
    fn clip_at_line_boundary() {
        let s = "aaaa aaaa\nbbbb bbbb\ncccc cccc\ndddd dddd".to_string();
        assert_eq!(clip(s.clone(), 1000), s);
        assert_eq!(clip(s.clone(), 8), "aaaa aaaa\n…[-9 tok]");
        assert_eq!(clip("x".repeat(400), 5), "x".repeat(400));
        assert_eq!(clip("é\nü\nö".to_string(), 3), "é\n…[-3 tok]");
    }
    #[test]
    fn facets_ordering_and_overflow() {
        let mut ps = vec![
            paper("1", "t", &["Terence Tao", "Ben Green"], &["math.NT", "math.CO"], "2023-01-03T00:00:00Z"),
            paper("2", "t", &["Terence Tao"], &["math.CO"], "2023-02-03T00:00:00Z"),
            paper("3", "t", &["Ann Zed"], &["math.AG"], "2022-02-03T00:00:00Z"),
        ];
        ps[1].venue = Some(Venue { name: "Acta", q: 1, area: "" });
        ps[1].jref = Some("Acta 1".into());
        let by = au(&["cat", "pcat", "year", "month", "au", "q", "venue", "pub", "bogus"]);
        assert_eq!(
            facets(&ps, &by),
            "cat: math.CO 2, math.AG 1, math.NT 1\npcat: math.AG 1, math.CO 1, math.NT 1\nyear: 2023 2, 2022 1\nmonth: 2022-02 1, 2023-01 1, 2023-02 1\nau: T. Tao 2, A. Zed 1, B. Green 1\nq: ? 2, Q1 1\nvenue: Acta 1\npub: - 2, P 1\nunknown facet \"bogus\"; valid: cat,pcat,year,month,au,q,venue,pub,inst"
        );
        let many: Vec<Paper> = (0..20).map(|i| paper("x", "t", &[], &[&format!("c.{i:02}")], "2020")).collect();
        let s = facets(&many, &au(&["cat"]));
        assert!(s.starts_with("cat: c.00 1, c.01 1,"));
        assert!(s.ends_with("c.14 1, +5"), "{s}");
    }
    #[test]
    fn items_render() {
        let it = vec![
            Item { kind: Kind::Problem, label: "Problem 3".into(), sec: "2.1".into(), text: "Show that\n  $x^2$ is\tbig.".into() },
            Item { kind: Kind::Conjecture, label: "".into(), sec: "".into(), text: "Every even number is a sum of two primes and then some more words follow".into() },
        ];
        let f = fmt(&["--abslen", "8"], "id");
        assert_eq!(items(&p1(), &it, &f), "2301.01234 Sums of two cubes\n[prob Problem 3 § 2.1] Show that $x^2$ is big.\n[conj] Every even number is a…");
        assert_eq!(items(&p1(), &[], &f), "2301.01234 Sums of two cubes\n0 items");
        let long = Paper { title: "w ".repeat(60), ..p2() };
        assert!(items(&long, &[], &f).lines().next().unwrap().chars().count() <= 12 + 1 + 70);
    }
    #[test]
    fn outline_indents_relative() {
        let s = |id: &str, level: u8, title: &str| Section { id: id.into(), level, title: title.into(), start: 0, end: 0 };
        let secs = vec![s("1", 2, "Intro"), s("1.1", 3, "Setup  here"), s("1.1.1", 4, "Deep"), s("2", 2, "Main")];
        assert_eq!(outline(&p2(), &secs), "math/0309285 On a conjecture\n1 Intro\n 1.1 Setup here\n  1.1.1 Deep\n2 Main");
        assert_eq!(outline(&p2(), &[]), "math/0309285 On a conjecture\n0 sections");
    }
    #[test]
    fn hits_render() {
        let h = vec![
            Hit { sec: "3.2".into(), pos: 1042, snippet: "the  cap set\nproblem".into() },
            Hit { sec: "".into(), pos: 7, snippet: "…already…".into() },
        ];
        assert_eq!(hits(&p2(), &h), "math/0309285 On a conjecture\n§3.2 @1042: …the cap set problem…\n@7: …already…");
        assert_eq!(hits(&p2(), &[]), "math/0309285 On a conjecture\n0 hits");
    }
    #[test]
    fn refs_render() {
        let r = vec![
            Ref { n: 1, text: "Green, Tao. Primes in progression. arXiv:math/0404188".into(), arxiv: Some("math/0404188".into()), doi: Some("10.4007/annals.2008.167.481".into()) },
            Ref { n: 2, text: "x".repeat(200), arxiv: None, doi: None },
        ];
        let out = refs(&p2(), &r, &fmt(&[], "id"));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "[1] Green, Tao. Primes in progression. arXiv:math/0404188 (doi:10.4007/annals.2008.167.481)");
        assert_eq!(lines[2], format!("[2] {}…", "x".repeat(159)));
        let full = refs(&p2(), &r, &fmt(&["--full"], "id"));
        assert!(full.ends_with(&"x".repeat(200)));
    }
    #[test]
    fn refs_truncated_keeps_identifier() {
        let r = vec![Ref { n: 3, text: format!("{} arXiv:1701.06538", "y".repeat(200)), arxiv: Some("1701.06538".into()), doi: None }];
        assert!(refs(&p2(), &r, &fmt(&[], "id")).ends_with(" (arXiv:1701.06538)"));
    }
    #[test]
    fn author_facet_does_not_merge_distinct_people() {
        let ps = vec![
            paper("1", "A", &["Alice Smith", "Zed Q"], &["math.NT"], "2025-01-01T00:00:00Z"),
            paper("2", "B", &["Adam Smith", "Zed Q"], &["math.NT"], "2025-01-01T00:00:00Z"),
        ];
        let s = facets(&ps, &au(&["au"]));
        assert!(s.contains("Alice Smith 1") && s.contains("Adam Smith 1") && s.contains("Z. Q 2"), "{s}");
    }

    #[test]
    fn clip_keeps_a_prefix_after_heading_lines() {
        let s = format!("2307.01234 Title\n## Introduction\n{}", "word ".repeat(2000));
        let c = clip(s, 100);
        assert!(c.contains("## Introduction\nword word"), "{c}");
    }

    #[test]
    fn clip_keeps_a_prefix_of_one_huge_body_line() {
        let s = format!("2307.01234 Title\n{}", "word ".repeat(2000));
        let c = clip(s, 100);
        assert!(c.starts_with("2307.01234 Title\nword word"), "{c}");
        assert!(c.contains("…\n…[-") && est_tokens(&c) <= 110, "{} tok", est_tokens(&c));
    }

    #[test]
    fn ids_format_is_strictly_ids() {
        let ps = vec![
            paper("2501.00001", "A", &["X Y"], &["math.NT"], "2025-01-01T00:00:00Z"),
            paper("2501.00002", "B", &["X Y"], &["math.NT"], "2025-01-02T00:00:00Z"),
        ];
        assert_eq!(papers(&ps, Some(50), &fmt(&["--fmt", "ids"], "id")), "2501.00001\n2501.00002");
        assert_eq!(papers(&ps, Some(50), &fmt(&["--fmt", "ids", "--tok", "1"], "id")), "2501.00001");
        assert_eq!(papers(&[], None, &fmt(&["--fmt", "ids"], "id")), "");
    }

    #[test]
    fn from_args_validation() {
        let v = |s: &[&str]| parse(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>()).unwrap();
        let e = Fmt::from_args(&v(&["--fields", "id,zz"]), "id").unwrap_err();
        assert!(e.contains("\"zz\"") && e.contains("id,ver,t,a,aff"), "{e}");
        let e = Fmt::from_args(&v(&["--fmt", "xml"]), "id").unwrap_err();
        assert!(e.contains("\"xml\"") && e.contains("tsv"), "{e}");
        assert!(Fmt::from_args(&v(&["--abslen", "x"]), "id").is_err());
        assert!(Fmt::from_args(&v(&["--tok", "0"]), "id").is_err());
        let f = Fmt::from_args(&v(&["--tok", "99", "--aumax", "5", "-f", "T,a,t"]), "id").unwrap();
        assert_eq!((f.tok, f.aumax(), f.abslen(), f.fields.len()), (Some(99), 5, 280, 2));
        let f = Fmt::from_args(&v(&["--full"]), "id").unwrap();
        assert_eq!((f.aumax(), f.abslen()), (usize::MAX, usize::MAX));
    }

    #[test]
    fn default_line_is_much_smaller_than_atom_and_json() {
        let mut p = p1();
        p.authors = au(&["Terence Tao", "Ben Green"]);
        p.title = "Sums of two cubes".into();
        p.abs = "We study representations of integers as sums of two cubes and prove an asymptotic formula.".into();
        let line = papers(&[p.clone()], None, &fmt(&[], "id,d,a,pc,t"));
        let atom = format!(
            "<entry>\n  <id>http://arxiv.org/abs/2301.01234v2</id>\n  <updated>2024-05-06T07:08:09Z</updated>\n  <published>2023-01-03T10:00:00Z</published>\n  <title>{}</title>\n  <summary>  {}\n</summary>\n  <author>\n    <name>Terence Tao</name>\n  </author>\n  <author>\n    <name>Ben Green</name>\n  </author>\n  <link href=\"http://arxiv.org/abs/2301.01234v2\" rel=\"alternate\" type=\"text/html\"/>\n  <link title=\"pdf\" href=\"http://arxiv.org/pdf/2301.01234v2\" rel=\"related\" type=\"application/pdf\"/>\n  <arxiv:primary_category xmlns:arxiv=\"http://arxiv.org/schemas/atom\" term=\"math.NT\" scheme=\"http://arxiv.org/schemas/atom\"/>\n  <category term=\"math.NT\" scheme=\"http://arxiv.org/schemas/atom\"/>\n  <category term=\"math.CO\" scheme=\"http://arxiv.org/schemas/atom\"/>\n</entry>",
            p.title, p.abs
        );
        let json = serde_json::json!({
            "id": "2301.01234", "title": p.title, "authors": ["Terence Tao", "Ben Green"], "summary": p.abs,
            "published": p.published, "updated": p.updated, "primary_category": "math.NT", "categories": ["math.NT", "math.CO"],
            "pdf_url": "http://arxiv.org/pdf/2301.01234v2", "links": ["http://arxiv.org/abs/2301.01234v2"]
        })
        .to_string();
        let (l, a, j) = (est_tokens(&line), est_tokens(&atom), est_tokens(&json));
        assert!(a * 2 >= l * 5, "atom {a} vs line {l}");
        assert!(j * 2 >= l * 5, "json {j} vs line {l}");
        assert_eq!(line, "2301.01234|2023-01|T. Tao, B. Green|math.NT|Sums of two cubes");
    }

    #[test]
    fn colliding_bib_keys_are_disambiguated() {
        let a = paper("2511.07721", "New results", &["Terence Tao"], &["math.NT"], "2025-11-01T00:00:00Z");
        let b = paper("2501.16779", "New bounds", &["Terence Tao"], &["math.NT"], "2025-01-01T00:00:00Z");
        let f = fmt(&["--fmt", "bib"], "id");
        let s = papers(&[a, b], None, &f);
        assert!(s.contains("@misc{tao2025new,") && s.contains("@misc{tao2025newb,"), "{s}");
    }

    #[test]
    fn bib_keys_unique_beyond_alphabet() {
        let ps: Vec<Paper> = (1..=60)
            .map(|i| paper(&format!("2501.{i:05}"), &format!("New result {i}"), &["Terence Tao"], &["math.NT"], "2025-01-01T00:00:00Z"))
            .collect();
        let s = papers(&ps, None, &fmt(&["--fmt", "bib"], "id"));
        let keys: Vec<&str> = s.lines().filter_map(|l| l.strip_prefix("@misc{")).map(|l| l.trim_end_matches(',')).collect();
        let uniq: std::collections::HashSet<&&str> = keys.iter().collect();
        assert_eq!((keys.len(), uniq.len()), (60, 60));
        assert_eq!(alpha_suffix(0), "b");
        assert_eq!(alpha_suffix(24), "z");
        assert_eq!(alpha_suffix(25), "bb");
    }
}
