//! Build arXiv API `search_query` + paging params from Args.

use crate::args::Args;
use crate::dates;
use crate::fuzzy::AuthorQ;
use crate::{tags, taxonomy};

const FIELDS: &[&str] = &["ti", "abs", "au", "cat", "co", "jr", "rn", "id", "all", "submittedDate", "lastUpdatedDate"];

pub struct Built {
    /// search_query (None when only --id is given).
    pub sq: Option<String>,
    pub ids: Vec<String>,
    pub sort_by: &'static str,
    pub order: &'static str,
    /// Query has author names with a full given name (so a relaxed retry can differ).
    pub has_strict_au: bool,
}

/// Split text into words / quoted phrases, preserving quotes, parens and operators as separate tokens.
fn toks(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut inq = false;
    let mut in_range = false; // `submittedDate:[202401010000 TO 202412312359]` stays one token
    for c in s.chars() {
        match c {
            '[' if !inq => {
                in_range = true;
                cur.push(c);
            }
            ']' if !inq => {
                in_range = false;
                cur.push(c);
            }
            c if c.is_whitespace() && in_range => cur.push(c),
            '"' => {
                cur.push('"');
                if inq {
                    out.push(std::mem::take(&mut cur));
                }
                inq = !inq;
            }
            c if c.is_whitespace() && !inq => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '(' | ')' if !inq => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(c.to_string());
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn clean_term(t: &str) -> String {
    t.chars().filter(|c| !matches!(c, '"' | '\\')).collect::<String>().trim().to_string()
}

/// `field:term` expression from free text: words ANDed, "quoted" phrases kept, AND/OR/ANDNOT/NOT/()
/// passed through, `ti:x` style prefixed tokens kept as-is when the field is known.
pub fn expr(field: &str, text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut need_op = false;
    for t in toks(text) {
        let up = t.to_ascii_uppercase();
        let piece = match up.as_str() {
            "AND" | "OR" | "ANDNOT" => {
                if need_op {
                    out.push(up);
                    need_op = false;
                }
                continue;
            }
            "NOT" => {
                if need_op {
                    out.push("ANDNOT".into());
                    need_op = false;
                }
                continue;
            }
            "(" => {
                if need_op {
                    out.push("AND".into());
                }
                out.push("(".into());
                need_op = false;
                continue;
            }
            ")" => {
                out.push(")".into());
                need_op = true;
                continue;
            }
            _ => {
                let pref = t.split_once(':').map(|(p, _)| p);
                if t.starts_with('"') {
                    format!("{field}:{t}")
                } else if pref.map_or(false, |p| FIELDS.contains(&p)) && !t.ends_with(':') {
                    t.clone()
                } else {
                    let c = clean_term(&t);
                    if c.is_empty() {
                        continue;
                    }
                    format!("{field}:{c}")
                }
            }
        };
        if need_op {
            out.push("AND".into());
        }
        out.push(piece);
        need_op = true;
    }
    while matches!(out.last().map(String::as_str), Some("AND" | "OR" | "ANDNOT")) {
        out.pop();
    }
    out.join(" ")
}

/// arXiv has no unary NOT: `NOT x` must follow a positive term (use --not for pure exclusions).
fn check_negation(text: &str) -> Result<(), String> {
    let mut prev_operand = false;
    for t in toks(text) {
        match t.to_ascii_uppercase().as_str() {
            "NOT" | "ANDNOT" if !prev_operand => return Err("NOT needs a term on its left (arXiv has no unary NOT); use --not WORDS for exclusions".into()),
            "AND" | "OR" | "ANDNOT" | "NOT" | "(" => prev_operand = false,
            _ => prev_operand = true,
        }
    }
    Ok(())
}

fn wrap(s: &str) -> String {
    if s.contains(' ') {
        format!("({s})")
    } else {
        s.to_string()
    }
}

fn or_join(v: Vec<String>) -> String {
    match v.len() {
        0 => String::new(),
        1 => v.into_iter().next().unwrap(),
        _ => format!("({})", v.join(" OR ")),
    }
}

fn cat_terms(items: &[String]) -> Result<Vec<String>, String> {
    let mut v = Vec::new();
    for it in items {
        let r = taxonomy::resolve(it);
        if r.is_empty() {
            return Err(format!("unknown category {it:?} (see `arx cats {it}`)"));
        }
        for c in r {
            let t = format!("cat:{c}");
            if !v.contains(&t) {
                v.push(t);
            }
        }
    }
    Ok(v)
}

/// `relax`: 0 = surname AND given name, 1 = surname only, 2 = surname prefix wildcard (typo fallback).
/// (arXiv sortBy, sortOrder) for a `--sort` value. `uold` is internal (alerts on --upd watches).
pub fn parse_sort(s: &str) -> Result<(&'static str, &'static str), String> {
    Ok(match s {
        "rel" | "relevance" => ("relevance", "descending"),
        "new" | "date" | "newest" => ("submittedDate", "descending"),
        "old" | "oldest" => ("submittedDate", "ascending"),
        "upd" | "updated" => ("lastUpdatedDate", "descending"),
        "uold" => ("lastUpdatedDate", "ascending"),
        o => return Err(format!("bad --sort {o:?} (rel|new|old|upd)")),
    })
}

pub fn build(a: &Args, relax: u8) -> Result<Built, String> {
    let now = dates::today();
    let text_all = std::iter::once(a.pos.join(" ")).chain(["ti", "abs", "all", "co", "jr"].iter().flat_map(|f| a.all(f).into_iter().map(String::from))).collect::<Vec<_>>();
    for t in &text_all {
        check_negation(t)?;
    }
    let mut and: Vec<String> = Vec::new();
    let mut not: Vec<String> = Vec::new();
    let mut has_strict_au = false;

    let text = a.pos.join(" ");
    if !text.trim().is_empty() {
        let e = expr("all", &text);
        if !e.is_empty() {
            and.push(wrap(&e));
        }
    }
    for (flag, field) in [("ti", "ti"), ("abs", "abs"), ("all", "all"), ("co", "co"), ("jr", "jr")] {
        for v in a.all(flag) {
            let e = expr(field, v);
            if !e.is_empty() {
                and.push(wrap(&e));
            }
        }
    }
    for g in a.groups("kw") {
        let mut v = Vec::new();
        for k in g {
            let c = clean_term(&k);
            if c.is_empty() {
                continue;
            }
            let q = if c.contains(char::is_whitespace) { format!("\"{c}\"") } else { c };
            v.push(format!("ti:{q}"));
            v.push(format!("abs:{q}"));
        }
        if !v.is_empty() {
            and.push(or_join(v));
        }
    }
    for g in a.groups("not") {
        for k in g {
            let c = clean_term(&k);
            if !c.is_empty() {
                let q = if c.contains(char::is_whitespace) { format!("\"{c}\"") } else { c };
                not.push(format!("(ti:{q} OR abs:{q})"));
            }
        }
    }
    for g in a.groups("au") {
        let mut v = Vec::new();
        for name in g {
            let q = AuthorQ::new(&name);
            let tok = q.api_token();
            if tok.is_empty() {
                continue;
            }
            let given = given_token(&name, &tok);
            if given.is_some() {
                has_strict_au = true;
            }
            match (&given, relax) {
                (Some(gv), 0) => v.push(format!("(au:{tok} AND au:{gv})")),
                (_, 2) => v.push(format!("au:{}", q.api_wild().unwrap_or(tok))),
                _ => v.push(format!("au:{tok}")),
            }
        }
        if !v.is_empty() {
            and.push(or_join(v));
        }
    }
    for g in a.groups("cat") {
        let t = cat_terms(&g)?;
        if !t.is_empty() {
            and.push(or_join(t));
        }
    }
    for g in a.groups("xcat") {
        for t in cat_terms(&g)? {
            not.push(t);
        }
    }
    for g in a.groups("tag") {
        let mut v = Vec::new();
        for n in g {
            let t = tags::tag(&n).ok_or_else(|| format!("unknown tag {n:?} (see `arx tags`)"))?;
            v.push(t.query.to_string());
        }
        and.push(or_join(v));
    }
    let date_field = if a.flag("upd") { "lastUpdatedDate" } else { "submittedDate" };
    // repeated bounds are AND: the latest lower and the earliest upper bound win
    let mut since: Option<dates::Ymd> = None;
    for s in a.all("since") {
        let d = dates::parse_bound(s, false, now)?;
        since = Some(since.map_or(d, |c| c.max(d)));
    }
    let mut until: Option<dates::Ymd> = None;
    for s in a.all("until") {
        let d = dates::parse_bound(s, true, now)?;
        until = Some(until.map_or(d, |c| c.min(d)));
    }
    if since.is_some() || until.is_some() {
        let lo = dates::api_stamp(since.unwrap_or((1991, 1, 1)), false);
        let hi = dates::api_stamp(until.unwrap_or(now), true);
        and.push(format!("{date_field}:[{lo} TO {hi}]"));
    }
    let ids: Vec<String> = a.list("id").into_iter().map(|s| norm_id(&s)).collect();

    let have_text = !and.is_empty();
    if !have_text && !not.is_empty() {
        and.push(format!("submittedDate:[199101010000 TO {}]", dates::api_stamp(now, true)));
    }
    let mut sq = and.join(" AND ");
    for n in &not {
        sq.push_str(" ANDNOT ");
        sq.push_str(n);
    }
    let sort = a.get("sort").unwrap_or(if have_text && has_text_terms(a) { "rel" } else { "new" });
    let (sort_by, order) = parse_sort(sort)?;
    Ok(Built { sq: if sq.is_empty() { None } else { Some(sq) }, ids, sort_by, order, has_strict_au })
}

fn has_text_terms(a: &Args) -> bool {
    !a.pos.is_empty() || a.has_any(&["ti", "abs", "all", "kw", "co", "jr", "tag"])
}

/// Full given name token (ASCII-folded) if the query has one ("Terence Tao", "Tao, Terence"); None for initials.
fn given_token(name: &str, surname_tok: &str) -> Option<String> {
    let folded = crate::fuzzy::fold(name);
    let parts: Vec<&str> = folded.split(' ').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    parts.iter().find(|p| p.len() >= 3 && **p != surname_tok && p.is_ascii() && !is_particle(p)).map(|p| p.to_string())
}

fn is_particle(p: &str) -> bool {
    matches!(p, "van" | "von" | "der" | "den" | "de" | "del" | "della" | "di" | "da" | "la" | "le" | "bin" | "ibn" | "al" | "el" | "du" | "dos" | "des")
}

/// "arXiv:2301.01234v2" / URLs -> "2301.01234" ; old-style ids kept.
pub fn norm_id(s: &str) -> String {
    let mut t = s.trim();
    for p in ["https://arxiv.org/abs/", "http://arxiv.org/abs/", "https://arxiv.org/pdf/", "http://arxiv.org/pdf/", "https://arxiv.org/html/", "arxiv.org/abs/", "arXiv:", "arxiv:"] {
        if let Some(r) = t.strip_prefix(p) {
            t = r;
            break;
        }
    }
    // drop URL query strings / fragments ("...abs/1706.03762?context=cs", "...html/1706.03762#S1")
    let t = t.split(['?', '#']).next().unwrap_or(t);
    let t = t.trim_end_matches(".pdf");
    let id = crate::atom::split_id(t).0;
    // "math.DG/0303109" (subject class inside an old-style id) -> canonical "math/0303109"
    match id.split_once('/') {
        Some((arch, num)) if arch.contains('.') => format!("{}/{num}", arch.split('.').next().unwrap_or(arch)),
        _ => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn q(s: &str) -> String {
        expr("all", s)
    }
    #[test]
    fn expr_basic() {
        assert_eq!(q("diffusion geometry"), "all:diffusion AND all:geometry");
        assert_eq!(q("\"functional equation\" OR diffusion"), "all:\"functional equation\" OR all:diffusion");
        assert_eq!(q("diffusion NOT survey"), "all:diffusion ANDNOT all:survey");
        assert_eq!(q("ti:ramsey cat:math.CO"), "ti:ramsey AND cat:math.CO");
        assert_eq!(q("a (b OR c)"), "all:a AND ( all:b OR all:c )");
        assert_eq!(q("OR a AND"), "all:a");
    }
    #[test]
    fn raw_date_ranges_stay_whole() {
        assert_eq!(q("submittedDate:[202401010000 TO 202412312359] diffusion"), "submittedDate:[202401010000 TO 202412312359] AND all:diffusion");
    }
    #[test]
    fn legacy_ids_with_subject_class() {
        assert_eq!(norm_id("math.DG/0303109"), "math/0303109");
        assert_eq!(norm_id("arXiv:hep-th/9901001v2"), "hep-th/9901001");
    }

    #[test]
    fn url_queries_and_fragments_are_dropped() {
        assert_eq!(norm_id("https://arxiv.org/abs/1706.03762?context=cs"), "1706.03762");
        assert_eq!(norm_id("https://arxiv.org/html/1706.03762v7#S1"), "1706.03762");
        assert_eq!(norm_id("https://arxiv.org/pdf/1706.03762v1.pdf#page=3"), "1706.03762");
    }

    #[test]
    fn ids_normalised() {
        assert_eq!(norm_id("arXiv:2301.01234v3"), "2301.01234");
        assert_eq!(norm_id("https://arxiv.org/pdf/2301.01234v1.pdf"), "2301.01234");
        assert_eq!(norm_id("math/0309285"), "math/0309285");
    }
}

#[cfg(test)]
mod build_tests {
    use super::*;
    use crate::args::parse;
    fn b(s: &[&str], relax: u8) -> Built {
        let v: Vec<String> = s.iter().map(|x| x.to_string()).collect();
        build(&parse(&v).unwrap(), relax).unwrap()
    }
    #[test]
    fn author_strict_then_relaxed() {
        let s = b(&["--au", "Terence Tao"], 0);
        assert_eq!(s.sq.as_deref(), Some("(au:tao AND au:terence)"));
        assert!(s.has_strict_au);
        assert_eq!(b(&["--au", "Terence Tao"], 1).sq.as_deref(), Some("au:tao"));
        assert_eq!(b(&["--au", "T. Tao"], 0).sq.as_deref(), Some("au:tao"));
    }
    #[test]
    fn cats_dates_not_and_sort() {
        let q = b(&["diffusion", "-c", "math.CO,math.NT", "--since", "2024", "--until", "2024-06", "--not", "survey"], 0);
        assert_eq!(
            q.sq.as_deref(),
            Some("all:diffusion AND (cat:math.CO OR cat:math.NT) AND submittedDate:[202401010000 TO 202406302359] ANDNOT (ti:survey OR abs:survey)")
        );
        assert_eq!((q.sort_by, q.order), ("relevance", "descending"));
        let n = b(&["-c", "math.CO"], 0);
        assert_eq!(n.sort_by, "submittedDate");
    }
    #[test]
    fn repeated_date_bounds_intersect() {
        let q = b(&["x", "--since", "2024", "--since", "2020", "--until", "2026", "--until", "2025-06"], 0);
        assert!(q.sq.unwrap().contains("submittedDate:[202401010000 TO 202506302359]"));
    }
    #[test]
    fn leading_not_is_rejected() {
        for q in ["NOT diffusion", "(NOT diffusion)", "diffusion AND NOT", "a OR NOT b"] {
            let v: Vec<String> = vec![q.to_string()];
            assert!(build(&parse(&v).unwrap(), 0).is_err(), "{q}");
        }
        let v: Vec<String> = vec!["diffusion NOT survey".to_string()];
        assert!(build(&parse(&v).unwrap(), 0).is_ok());
    }
    #[test]
    fn errors() {
        let v: Vec<String> = ["-c", "zzzz"].iter().map(|x| x.to_string()).collect();
        assert!(build(&parse(&v).unwrap(), 0).is_err());
        let v: Vec<String> = ["x", "--sort", "bogus"].iter().map(|x| x.to_string()).collect();
        assert!(build(&parse(&v).unwrap(), 0).is_err());
    }
    #[test]
    fn only_negative_gets_base_clause() {
        let q = b(&["--xcat", "cs.*"], 0);
        assert!(q.sq.unwrap().starts_with("submittedDate:[199101010000 TO "));
    }
}
