//! Commands. Each takes parsed Args plus optional piped papers and returns rendered text + papers.

use futures_util::future::join_all;

use crate::args::Args;
use crate::engine;
use crate::filter::Filter;
use crate::model::*;
use crate::net::{Net, Ttl};
use crate::out::{self, Fmt};
use crate::{html, query, tags, taxonomy, tex, venue};

/// Alert cursor update: (watch name, (cursor date, seen ids), (query, last date) snapshot it was computed from).
pub type Cursor = (String, (String, Vec<String>), (String, String));

#[derive(Default)]
pub struct Out {
    /// Alert cursors to persist once the whole statement succeeded and its output was written.
    pub cursors: Vec<Cursor>,
    pub papers: Vec<Paper>,
    pub text: String,
    /// Number of sub-requests (papers/files) that failed; the text still carries partial results.
    pub failed: usize,
}

impl Out {
    fn text(t: impl Into<String>) -> Out {
        Out { text: t.into(), ..Default::default() }
    }
}

pub type Res = Result<Out, String>;
pub type Input = Option<Vec<Paper>>;

pub fn looks_like_id(s: &str) -> bool {
    let t = query::norm_id(s);
    let b = t.as_bytes();
    // new style: 4 digits '.' 4-5 digits
    if let Some((l, r)) = t.split_once('.') {
        return l.len() == 4 && l.bytes().all(|c| c.is_ascii_digit()) && (4..=5).contains(&r.len()) && r.bytes().all(|c| c.is_ascii_digit());
    }
    // old style: archive/YYMMNNN
    if let Some((l, r)) = t.split_once('/') {
        return !l.is_empty() && r.len() == 7 && r.bytes().all(|c| c.is_ascii_digit()) && b[0].is_ascii_alphabetic();
    }
    false
}

fn ids_from(a: &Args) -> Vec<String> {
    let mut v: Vec<String> = a.pos.iter().filter(|s| looks_like_id(s)).map(|s| query::norm_id(s)).collect();
    v.extend(a.list("id").into_iter().map(|s| query::norm_id(&s)));
    let mut seen = std::collections::HashSet::new();
    v.retain(|i| seen.insert(i.clone()));
    v
}

/// Positional args that are not ids (query words / patterns).
fn non_id_pos(a: &Args) -> Vec<String> {
    a.pos.iter().filter(|s| !looks_like_id(s)).cloned().collect()
}

/// With --inst active, show only the institutions that satisfied the filter (not every co-author's).
fn keep_matching_insts(a: &Args, papers: &mut [Paper]) {
    let terms = a.list("inst");
    if terms.is_empty() {
        return;
    }
    let thr = a.num::<f32>("fuzz").unwrap_or(0.82);
    for p in papers.iter_mut() {
        let hit: Vec<String> = p
            .insts
            .iter()
            .chain(&p.affs)
            .filter(|i| terms.iter().any(|t| crate::fuzzy::contains_fuzzy_strict(i, t) >= thr))
            .cloned()
            .collect();
        p.insts = hit;
    }
}

/// Lines that report a failed sub-request (`id: error: ...` / `id: not found`).
fn count_errors(s: &str) -> usize {
    s.lines().filter(|l| l.contains(": error: ") || l.ends_with(": not found")).count()
}

fn stub_paper(id: &str, title: &str) -> Paper {
    Paper { id: id.to_string(), title: title.to_string(), ..Default::default() }
}

fn want_display_enrich(a: &Args) -> (bool, bool) {
    let f = a.get("fields").unwrap_or("");
    let has = |c: &str| f.split(',').any(|x| x.trim().eq_ignore_ascii_case(c));
    let facet_inst = a.list("facet").iter().any(|x| x == "inst");
    (has("inst") || facet_inst, has("msc"))
}

/// Fetch institutions / MSC classes when --fields or --facet ask for them (no-op when already enriched).
pub async fn enrich_for_display(net: &Net, a: &Args, papers: &mut [Paper]) -> Result<(), String> {
    let (ei, em) = want_display_enrich(a);
    engine::enrich(net, papers, ei, em).await
}

fn default_fields(a: &Args, base: &str) -> String {
    let mut f = base.to_string();
    if a.has_any(&["q", "published", "jn"]) {
        f.push_str(",q,v");
    } else if a.has_any(&["preprint", "accepted"]) {
        f.push_str(",pub");
    }
    if a.has_any(&["inst"]) {
        f.push_str(",inst");
    }
    if a.has_any(&["cmt", "pages"]) {
        f.push_str(",co");
    }
    if a.has_any(&["msc"]) {
        f.push_str(",msc");
    }
    f
}

/// Render a paper list (--tok handled inside `out::papers`, which clips on whole records).
pub fn render(a: &Args, papers: &[Paper], total: Option<usize>) -> Result<String, String> {
    let fmt = Fmt::from_args(a, &default_fields(a, "id,d,a,pc,t"))?;
    // --inst narrows the *displayed* institutions only; the caller's papers stay complete
    if a.flag("inst") {
        let mut shown = papers.to_vec();
        keep_matching_insts(a, &mut shown);
        return Ok(out::papers(&shown, total, &fmt));
    }
    Ok(out::papers(papers, total, &fmt))
}

const SERVER_TERMS: &[&str] = &["ti", "abs", "kw", "not", "all", "co", "jr", "au", "cat", "xcat", "tag", "since", "until"];

/// Query-side constraints present (besides explicit ids)?
fn has_server_terms(a: &Args) -> bool {
    !non_id_pos(a).is_empty() || a.has_any(SERVER_TERMS)
}

/// Piped papers from content commands are id-only stubs; fill their metadata with one batched call.
async fn hydrate(net: &Net, papers: Vec<Paper>) -> Result<Vec<Paper>, String> {
    let missing: Vec<String> = papers.iter().filter(|p| p.title.is_empty()).map(|p| p.id.clone()).collect();
    if missing.is_empty() {
        return Ok(papers);
    }
    let mut full: std::collections::HashMap<String, Paper> = engine::fetch_ids(net, &missing).await?.into_iter().map(|p| (p.id.clone(), p)).collect();
    Ok(papers.into_iter().map(|p| if p.title.is_empty() { full.remove(&p.id).unwrap_or(p) } else { p }).collect())
}

/// Papers for a command: piped input > explicit ids > query search. Filters always apply.
async fn papers_for(net: &Net, a: &Args, input: Input) -> Result<(Vec<Paper>, Option<usize>), String> {
    // an empty piped list means "nothing to process" (not "search from scratch"), but options still validate
    if input.as_ref().is_some_and(Vec::is_empty) {
        Filter::from_args(a)?;
        query::build(a, 0)?; // syntax errors (bad category, tag, date, NOT) surface even with nothing to search
        if let Some(s) = a.get("sort") {
            query::parse_sort(s)?;
        }
        return Ok((Vec::new(), None));
    }
    let ids = ids_from(a);
    let piped = input;
    let ids = match &piped {
        Some(p) if has_server_terms(a) => p.iter().map(|x| x.id.clone()).collect(),
        _ => ids,
    };
    let mut papers = if let (Some(p), false) = (piped.clone(), has_server_terms(a)) {
        hydrate(net, p).await?
    } else if !ids.is_empty() && !has_server_terms(a) {
        engine::fetch_ids(net, &ids).await?
    } else {
        // ids (positional or --id) + query terms: let the API intersect them
        let mut b = a.clone();
        b.pos.retain(|s| !looks_like_id(s));
        b.kv.retain(|(k, _)| *k != "id");
        if !ids.is_empty() {
            b.kv.push(("id", ids.join(",")));
        }
        let f = engine::search(net, &b).await?;
        return Ok((f.papers, f.total));
    };
    let filter = Filter::from_args(a)?;
    if !filter.any() {
        // classification only (venue/quartile for display); nothing is dropped
        for p in &mut papers {
            filter.pass_meta(p);
        }
    }
    if filter.any() {
        papers.retain_mut(|p| filter.pass_meta(p));
        engine::enrich(net, &mut papers, filter.needs_insts(), filter.needs_msc()).await?;
        papers.retain(|p| filter.pass_enriched(p));
    }
    // explicit date ordering applies to piped / id lists too, before skip and -n
    if let Some(sort) = a.get("sort") {
        let (by, order) = query::parse_sort(sort)?;
        engine::sort_local(&mut papers, by, order);
    }
    let skip = a.num::<usize>("skip").unwrap_or(0);
    let mut papers: Vec<Paper> = papers.into_iter().skip(skip).collect();
    if let Some(n) = a.num::<usize>("n") {
        papers.truncate(n);
    }
    Ok((papers, None))
}

fn args_with(a: &Args, extra: &[(&'static str, &str)]) -> Args {
    let mut b = a.clone();
    for (k, v) in extra {
        if !b.flag(k) {
            b.kv.push((k, v.to_string()));
        }
    }
    b
}

// ---------------------------------------------------------------- search/get/bib/comments

pub async fn search(net: &Net, a: &Args, input: Input) -> Res {
    let mut a = a.clone();
    let facet = a.list("facet");
    if !facet.is_empty() && !a.flag("n") {
        let scan = a.get("scan").unwrap_or("400").to_string();
        a.kv.push(("n", scan));
    }
    let (mut papers, total) = papers_for(net, &a, input).await?;
    enrich_for_display(net, &a, &mut papers).await?;
    if !facet.is_empty() {
        Fmt::from_args(&a, "id")?; // validates --tok like every other output path
        let mut shown = papers.clone();
        keep_matching_insts(&a, &mut shown); // --inst facets count only the institutions that matched
        let text = clip_opt(&a, out::facets(&shown, &facet));
        return Ok(Out { text, papers, ..Default::default() });
    }
    let text = render(&a, &papers, total)?;
    Ok(Out { papers, text, ..Default::default() })
}

pub async fn get(net: &Net, a: &Args, input: Input) -> Res {
    if ids_from(a).is_empty() && input.is_none() {
        return Err("get: give arXiv ids (2301.01234 math/0309285 ...)".into());
    }
    let a = args_with(a, &[("fields", &default_fields(a, "id,d,a,c,t,jr,co,ab"))]);
    search(net, &a, input).await
}

pub async fn comments(net: &Net, a: &Args, input: Input) -> Res {
    let (papers, _) = papers_for(net, a, input).await?;
    let mut s = String::new();
    let mut none = 0;
    for p in &papers {
        match &p.comment {
            Some(c) => {
                s.push_str(&p.id);
                s.push(' ');
                s.push_str(c);
                s.push('\n');
            }
            None => none += 1,
        }
    }
    if none > 0 {
        s.push_str(&format!("({none} without comment)\n"));
    }
    if papers.is_empty() {
        s.push_str("0 results\n");
    }
    let t = a.num::<usize>("tok");
    let s = s.trim_end().to_string();
    Ok(Out { papers, text: t.map_or(s.clone(), |t| out::clip(s, t)), ..Default::default() })
}

// ---------------------------------------------------------------- doc loading

pub struct DocData {
    pub title: String,
    pub text: String,
    pub sections: Vec<Section>,
    pub items: Vec<Item>,
    pub refs: Vec<Ref>,
    /// `text` holds raw LaTeX (needs cleaning before display) instead of Markdown.
    pub is_tex: bool,
}

async fn load_html(net: &Net, id: &str) -> Result<Option<DocData>, String> {
    for base in ["https://arxiv.org/html/", "https://ar5iv.labs.arxiv.org/html/"] {
        if let Some(h) = net.get_text(&format!("{base}{id}"), Ttl::Doc).await? {
            let d = html::parse(&h);
            return Ok(Some(DocData { title: d.title, text: d.md, sections: d.sections, items: d.items, refs: d.refs, is_tex: false }));
        }
    }
    Ok(None)
}

async fn load_tex(net: &Net, id: &str) -> Result<Option<DocData>, String> {
    let Some(b) = net.get_bytes(&format!("https://arxiv.org/e-print/{id}"), Ttl::Doc).await? else { return Ok(None) };
    let files = tex::unpack(&b)?;
    let Some(t) = tex::flatten(&files) else { return Ok(None) };
    let sections = tex::outline(&t);
    let items = tex::items(&t);
    let refs = tex::refs(&t);
    Ok(Some(DocData { title: String::new(), text: t, sections, items, refs, is_tex: true }))
}

/// `--src html` (default, falls back to tex when no HTML exists) or `--src tex`.
pub async fn load_doc(net: &Net, id: &str, a: &Args) -> Result<DocData, String> {
    let want_tex = a.get("src") == Some("tex");
    let d = if want_tex {
        load_tex(net, id).await?
    } else {
        match load_html(net, id).await? {
            Some(d) => Some(d),
            None => load_tex(net, id).await?,
        }
    };
    d.ok_or_else(|| format!("{id}: no HTML or TeX source available"))
}

/// Run `f` over every id concurrently; results in input order, errors inlined as text.
async fn per_doc<F>(net: &Net, a: &Args, ids: Vec<String>, f: F) -> (String, usize)
where
    F: Fn(&str, DocData) -> String,
{
    let futs = ids.iter().map(|id| async move { (id, load_doc(net, id, a).await) });
    let mut s = String::new();
    let mut failed = 0;
    for (id, r) in join_all(futs).await {
        let part = match r {
            Ok(d) => {
                let p = f(id, d);
                // callbacks report a per-document problem as "<id>: error: ..." (e.g. a missing section)
                if p.starts_with(&format!("{id}: error:")) {
                    failed += 1;
                }
                p
            }
            Err(e) => {
                failed += 1;
                format!("{id}: error: {e}")
            }
        };
        if !part.is_empty() {
            s.push_str(&part);
            if !part.ends_with('\n') {
                s.push('\n');
            }
        }
    }
    (s.trim_end().to_string(), failed)
}

/// Ids for content commands: piped papers or explicit ids; a text query triggers a search.
async fn content_ids(net: &Net, a: &Args, input: Input) -> Result<(Vec<Paper>, Vec<String>), String> {
    let ids = ids_from(a);
    // fast path: bare ids need no metadata round trip (filters/query terms force the full path)
    if input.is_none() && !ids.is_empty() && !has_server_terms(a) && !a.flag("sort") && !Filter::from_args(a)?.any() {
        let skip = a.num::<usize>("skip").unwrap_or(0);
        let ids: Vec<String> = ids.into_iter().skip(skip).take(a.num::<usize>("n").unwrap_or(usize::MAX)).collect();
        let ps = ids.iter().map(|i| stub_paper(i, "")).collect();
        return Ok((ps, ids));
    }
    let (ps, _) = papers_for(net, a, input).await?;
    let ids = ps.iter().map(|p| p.id.clone()).collect();
    Ok((ps, ids))
}

fn clip_opt(a: &Args, s: String) -> String {
    match a.num::<usize>("tok") {
        Some(t) => out::clip(s, t),
        None => s,
    }
}

// ---------------------------------------------------------------- theorem-like blocks

fn parse_kinds(spec: Option<&str>) -> Result<Vec<Kind>, String> {
    const ALL: &[Kind] = &[
        Kind::Problem, Kind::Conjecture, Kind::Question, Kind::Exercise, Kind::Theorem, Kind::Lemma, Kind::Proposition,
        Kind::Corollary, Kind::Definition, Kind::Remark, Kind::Example, Kind::Claim, Kind::Other,
    ];
    let spec = spec.unwrap_or("prob,conj,q");
    let mut v = Vec::new();
    for t in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if t == "all" {
            return Ok(ALL.to_vec());
        }
        let k = ALL.iter().find(|k| k.code() == t || format!("{k:?}").to_ascii_lowercase() == t).ok_or_else(|| {
            format!("bad --kind {t:?}; use {} or all", ALL.iter().map(|k| k.code()).collect::<Vec<_>>().join(","))
        })?;
        v.push(*k);
    }
    Ok(v)
}

fn render_items(id: &str, d: &DocData, kinds: &[Kind], a: &Args, f: &Fmt, meta: Option<&Paper>, show_empty: bool) -> String {
    let pat = a.get("pat").map(str::to_ascii_lowercase);
    let max = a.num::<usize>("max").unwrap_or(usize::MAX);
    let items: Vec<Item> = d
        .items
        .iter()
        .filter(|i| kinds.contains(&i.kind))
        .filter(|i| pat.as_ref().map_or(true, |p| i.text.to_ascii_lowercase().contains(p.as_str())))
        .take(max)
        .cloned()
        .collect();
    let stub;
    let p = match meta {
        Some(p) => p,
        None => {
            stub = stub_paper(id, &d.title);
            &stub
        }
    };
    if items.is_empty() {
        return if show_empty { format!("{id} 0 items") } else { String::new() };
    }
    out::items(p, &items, f)
}

pub async fn problems(net: &Net, a: &Args, input: Input) -> Res {
    let kinds = parse_kinds(a.get("kind"))?;
    let f = Fmt::from_args(a, "id,t")?;
    let (papers, ids) = content_ids(net, a, input).await?;
    if ids.is_empty() {
        return Ok(Out::text("0 results"));
    }
    let metas: std::collections::HashMap<&str, &Paper> = papers.iter().map(|p| (p.id.as_str(), p)).collect();
    let (text, failed) = per_doc(net, a, ids, |id, d| {
        let m = metas.get(id).copied().filter(|p| !p.title.is_empty());
        render_items(id, &d, &kinds, a, &f, m, true)
    })
    .await;
    Ok(Out { papers, text: clip_opt(a, text), failed, ..Default::default() })
}

// ---------------------------------------------------------------- text / outline / sec / grep / refs

const DEFAULT_TEXT_TOK: usize = 6000;

pub async fn text(net: &Net, a: &Args, input: Input) -> Res {
    // --skip is a character offset here, not a paper offset
    let mut sel = a.clone();
    sel.kv.retain(|(k, _)| *k != "skip");
    let (papers, ids) = content_ids(net, &sel, input).await?;
    let sec = a.get("sec").map(str::to_string);
    let tok = a.num::<usize>("tok").unwrap_or(DEFAULT_TEXT_TOK);
    let skip = a.num::<usize>("skip").unwrap_or(0);
    let (s, failed) = per_doc(net, a, ids, |id, d| {
        let region: &str = match &sec {
            Some(k) => match html::find_section(&d.sections, k) {
                Some(s) => d.text.get(s.start..s.end).unwrap_or(""),
                None => return format!("{id}: error: no section {k:?} (see `arx outline {id}`)"),
            },
            // raw TeX: skip the preamble, the typeset body starts at \begin{document}
            None if d.is_tex => tex::body(&d.text),
            None => &d.text,
        };
        let cleaned;
        let body: &str = if d.is_tex {
            cleaned = tex::clean(region);
            &cleaned
        } else {
            region
        };
        let start = body.char_indices().nth(skip).map_or(body.len(), |(i, _)| i);
        format!("{id}{}\n{}", if d.title.is_empty() { String::new() } else { format!(" {}", d.title) }, &body[start..])
    })
    .await;
    Ok(Out { papers, text: out::clip(s, tok), failed, ..Default::default() })
}

pub async fn outline(net: &Net, a: &Args, input: Input) -> Res {
    let (papers, ids) = content_ids(net, a, input).await?;
    let (s, failed) = per_doc(net, a, ids, |id, d| out::outline(&stub_paper(id, &d.title), &d.sections)).await;
    Ok(Out { papers, text: clip_opt(a, s), failed, ..Default::default() })
}

pub async fn grep(net: &Net, a: &Args, input: Input) -> Res {
    let mut a = a.clone();
    if !a.flag("pat") {
        // the pattern is the last positional that is not an id; when every positional looks like an id
        // (grepping for a citation like 1706.03762) the last one is the pattern if something else
        // (another id or piped papers) supplies the targets
        let pos = a.pos.clone();
        let idx = pos
            .iter()
            .rposition(|s| !looks_like_id(s))
            .or_else(|| (pos.len() >= 2 || ((input.is_some() || a.flag("id")) && !pos.is_empty())).then(|| pos.len() - 1));
        let Some(i) = idx else { return Err("grep: give a pattern (arx grep ID PATTERN)".into()) };
        a.kv.push(("pat", pos[i].clone()));
        a.pos.remove(i);
        a.pos.retain(|s| looks_like_id(s));
    }
    let pat = a.get("pat").unwrap().to_string();
    let ctx = a.num::<usize>("ctx").unwrap_or(120);
    let max = a.num::<usize>("max").unwrap_or(5);
    let (papers, ids) = content_ids(net, &a, input).await?;
    let (s, failed) = per_doc(net, &a, ids, |id, d| {
        // raw TeX is searched as the cleaned typeset text (no preamble, no `\%` escapes); no section labels there
        let cleaned;
        let (text, secs): (&str, &[Section]) = if d.is_tex {
            cleaned = tex::clean(tex::body(&d.text));
            (&cleaned, &[])
        } else {
            (&d.text, &d.sections)
        };
        let hits = html::grep(text, secs, &pat, ctx, max);
        if hits.is_empty() {
            return String::new();
        }
        out::hits(&stub_paper(id, &d.title), &hits)
    })
    .await;
    let s = if s.is_empty() { "0 hits".to_string() } else { s };
    Ok(Out { papers, text: clip_opt(&a, s), failed, ..Default::default() })
}

pub async fn refs(net: &Net, a: &Args, input: Input) -> Res {
    let (papers, ids) = content_ids(net, a, input).await?;
    let f = Fmt::from_args(a, "id")?;
    let pat = a.get("pat").map(str::to_ascii_lowercase);
    let max = a.num::<usize>("max").unwrap_or(usize::MAX);
    let (s, failed) = per_doc(net, a, ids, |id, d| {
        let r: Vec<Ref> = d
            .refs
            .iter()
            .filter(|r| pat.as_ref().map_or(true, |p| r.text.to_ascii_lowercase().contains(p.as_str())))
            .take(max)
            .cloned()
            .collect();
        out::refs(&stub_paper(id, &d.title), &r, &f)
    })
    .await;
    Ok(Out { papers, text: clip_opt(a, s), failed, ..Default::default() })
}

// ---------------------------------------------------------------- versions / cites

pub async fn versions(net: &Net, a: &Args, input: Input) -> Res {
    let (papers, ids) = content_ids(net, a, input).await?;
    let futs = ids.iter().map(|id| async move {
        let r = net.get_text(&format!("https://arxiv.org/abs/{id}"), Ttl::Doc).await;
        (id, r)
    });
    let mut s = String::new();
    for (id, r) in join_all(futs).await {
        match r {
            Ok(Some(h)) => {
                let m = html::abs_meta(&h);
                s.push_str(id);
                for (v, d, sz) in m.versions {
                    s.push_str(&format!(" {v} {d} {sz}"));
                }
                s.push('\n');
            }
            Ok(None) => s.push_str(&format!("{id}: not found\n")),
            Err(e) => s.push_str(&format!("{id}: error: {e}\n")),
        }
    }
    {
        let text = s.trim_end().to_string();
        Ok(Out { papers, failed: count_errors(&text), text: clip_opt(a, text), ..Default::default() })
    }
}

/// Citing papers via Semantic Scholar (optional external service; arXiv has no citation graph).
pub async fn cites(net: &Net, a: &Args, input: Input) -> Res {
    let (papers, ids) = content_ids(net, a, input).await?;
    let max = a.num::<usize>("max").unwrap_or(15).min(1000);
    let dir = if a.get("src") == Some("refs") { "references" } else { "citations" };
    let futs = ids.iter().map(|id| async move {
        let url = format!(
            "https://api.semanticscholar.org/graph/v1/paper/arXiv:{id}/{dir}?fields=title,year,venue,externalIds,citationCount&limit={max}"
        );
        (id, net.get_text(&url, Ttl::Doc).await)
    });
    let mut s = String::new();
    for (id, r) in join_all(futs).await {
        match r {
            Ok(Some(body)) => {
                let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("{id}: bad json: {e}"))?;
                let key = if dir == "citations" { "citingPaper" } else { "citedPaper" };
                let data = v["data"].as_array().cloned().unwrap_or_default();
                s.push_str(&format!("{id} {dir} ({})\n", data.len()));
                for d in data {
                    let p = &d[key];
                    let ax = p["externalIds"]["ArXiv"].as_str().unwrap_or("");
                    s.push_str(&format!(
                        "{}|{}|{}|{}|c{}\n",
                        ax,
                        p["year"].as_i64().map_or(String::new(), |y| y.to_string()),
                        p["title"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" "),
                        p["venue"].as_str().unwrap_or(""),
                        p["citationCount"].as_i64().unwrap_or(0)
                    ));
                }
            }
            Ok(None) => s.push_str(&format!("{id}: not in Semantic Scholar\n")),
            Err(e) => s.push_str(&format!("{id}: error: {e}\n")),
        }
    }
    {
        let text = s.trim_end().to_string();
        Ok(Out { papers, failed: count_errors(&text), text: clip_opt(a, text), ..Default::default() })
    }
}

// ---------------------------------------------------------------- reference lookups

/// Catalogue output honours --tok like every other path.
fn catalogue(a: &Args, text: String) -> Res {
    Fmt::from_args(a, "id")?;
    Ok(Out::text(clip_opt(a, text)))
}

pub fn cats(a: &Args) -> Res {
    catalogue(a, taxonomy::list(a.pos.first().map(String::as_str)))
}

pub fn tag_list(a: &Args) -> Res {
    catalogue(a, tags::list_tags())
}

pub fn venue_cmd(a: &Args) -> Res {
    if a.pos.is_empty() {
        // same --q grammar as the search filter (lists, ranges, repeats intersect)
        let mut mask = 0b1111u8;
        for s in a.all("q") {
            mask &= crate::filter::parse_quartiles(s).map_err(|e| format!("--q: {e}"))?;
        }
        return catalogue(a, venue::list(mask, a.get("cat")));
    }
    let name = a.pos.join(" ");
    let hits = venue::lookup(&name, 5);
    if hits.is_empty() {
        return catalogue(a, format!("{name:?}: unknown venue (add with --qfile)"));
    }
    let s = hits.iter().map(|(v, sc)| format!("Q{} {} [{}] {:.2}", v.q, v.name, v.area, sc)).collect::<Vec<_>>().join("\n");
    catalogue(a, s)
}

// ---------------------------------------------------------------- related / dl

const STOP: &[&str] = &[
    "a", "an", "the", "of", "on", "in", "for", "and", "or", "to", "with", "via", "by", "from", "is", "are", "at", "as", "its", "their", "using",
    "new", "towards", "toward", "into", "about", "between", "over", "under", "we", "our", "study", "note", "some", "case", "approach",
];

/// Papers similar to the given ids: primary category + salient title words, ranked by arXiv relevance.
pub async fn related(net: &Net, a: &Args, input: Input) -> Res {
    // --skip pages through the related results, not through the seed list
    // seeds are the given ids only: result filters (--since, --nau, ...) and paging apply to the related list
    let mut sel = Args { pos: a.pos.clone(), kv: Vec::new() };
    sel.kv.extend(a.kv.iter().filter(|(k, _)| *k == "id").cloned());
    let (papers, ids) = content_ids(net, &sel, input).await?;
    let base = if papers.iter().all(|p| p.title.is_empty()) { engine::fetch_ids(net, &ids).await? } else { papers };
    let mut out_papers: Vec<Paper> = Vec::new();
    let mut text = String::new();
    for p in &base {
        let words: Vec<String> = crate::fuzzy::fold(&p.title)
            .split(' ')
            .filter(|w| w.len() > 2 && !STOP.contains(w))
            .map(str::to_string)
            .take(8)
            .collect();
        if words.is_empty() {
            continue;
        }
        let mut b = a.clone();
        b.pos = vec![format!("cat:{} AND ({})", p.primary(), words.iter().map(|w| format!("ti:{w} OR abs:{w}")).collect::<Vec<_>>().join(" OR "))];
        b.kv.retain(|(k, _)| *k != "id");
        let want = a.num::<usize>("n").unwrap_or(5).clamp(1, engine::MAX_ROWS);
        let skip = a.num::<usize>("skip").unwrap_or(0).min(engine::MAX_ROWS);
        // fetch skip + n + 1 rows (the seed itself may rank first), drop the seed, then page
        b.kv.retain(|(k, _)| *k != "n" && *k != "skip");
        b.kv.push(("n", (skip + want + 1).min(engine::MAX_ROWS).to_string()));
        let b = args_with(&b, &[("sort", "rel")]);
        let mut found = engine::search(net, &b).await?.papers;
        found.retain(|q| q.id != p.id);
        let mut found: Vec<Paper> = found.into_iter().skip(skip).take(want).collect();
        enrich_for_display(net, &b, &mut found).await?;
        text.push_str(&format!("~{}\n{}\n", p.id, render(&b, &found, None)?));
        out_papers.extend(found);
    }
    // each seed's list already honours --tok (whole records); re-clipping would cut BibTeX/JSON entries
    Ok(Out { papers: out_papers, text: text.trim_end().to_string(), ..Default::default() })
}

/// Download pdf (default) / tex source / html to --to DIR (default "."): concurrent, prints paths.
pub async fn download(net: &Net, a: &Args, input: Input) -> Res {
    let (papers, ids) = content_ids(net, a, input).await?;
    let dir = std::path::PathBuf::from(a.get("to").unwrap_or("."));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let kind = a.get("src").unwrap_or("pdf");
    let (url_of, ext): (fn(&str) -> String, &str) = match kind {
        "pdf" => (|id| format!("https://arxiv.org/pdf/{id}"), "pdf"),
        "tex" => (|id| format!("https://arxiv.org/e-print/{id}"), "tar.gz"),
        "html" => (|id| format!("https://arxiv.org/html/{id}"), "html"),
        o => return Err(format!("bad --src {o:?} for dl (pdf|tex|html)")),
    };
    let futs = ids.iter().map(|id| {
        let dir = dir.clone();
        async move {
            let r = net.get_bytes(&url_of(id), Ttl::Doc).await;
            let path = dir.join(format!("{}.{ext}", id.replace('/', "_")));
            match r {
                Ok(Some(b)) => match std::fs::write(&path, &b) {
                    Ok(()) => format!("{} {}KB", path.display(), b.len() / 1024),
                    Err(e) => format!("{id}: error: {e}"),
                },
                Ok(None) => format!("{id}: not found"),
                Err(e) => format!("{id}: error: {e}"),
            }
        }
    });
    let text = join_all(futs).await.join("\n");
    Ok(Out { papers, failed: count_errors(&text), text, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn related_seeds_ignore_result_filters() {
        let a = crate::args::parse(&["1706.03762".to_string(), "--since".into(), "2024".into(), "--nau".into(), "1".into()]).unwrap();
        let mut sel = Args { pos: a.pos.clone(), kv: Vec::new() };
        sel.kv.extend(a.kv.iter().filter(|(k, _)| *k == "id").cloned());
        assert_eq!(ids_from(&sel), ["1706.03762"]);
        assert!(!has_server_terms(&sel) && !Filter::from_args(&sel).unwrap().any());
    }
    #[test]
    fn id_detection() {
        for ok in ["2301.01234", "2301.0123", "arXiv:2301.01234v2", "https://arxiv.org/abs/2301.01234", "math/0309285", "hep-th/9901001v1"] {
            assert!(looks_like_id(ok), "{ok}");
        }
        for bad in ["diffusion", "2024", "12.34", "math.CO", "1234.567890", "functional"] {
            assert!(!looks_like_id(bad), "{bad}");
        }
    }
    #[test]
    fn kinds() {
        assert_eq!(parse_kinds(None).unwrap().len(), 3);
        assert_eq!(parse_kinds(Some("thm,lem")).unwrap(), [Kind::Theorem, Kind::Lemma]);
        assert_eq!(parse_kinds(Some("all")).unwrap().len(), 13);
        assert!(parse_kinds(Some("zzz")).unwrap_err().contains("prob"));
    }
}
