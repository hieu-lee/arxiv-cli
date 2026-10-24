//! Alternative metadata sources for when arXiv is throttling or down: OpenAlex (generous limits, and it
//! knows author institutions). Results lack arXiv categories and author comments; every use is announced
//! with a note so the caller knows the answer is degraded.

use crate::args::Args;
use crate::engine::Found;
use crate::filter::Filter;
use crate::model::Paper;
use crate::net::{Net, Ttl};
use crate::{dates, openalex};

const OA_IDS_BATCH: usize = 50;
const OA_PAGE: usize = 100;
const OA_MAX_WINDOW: usize = 10_000;

fn auth(net: &Net) -> openalex::Auth<'_> {
    openalex::Auth { api_key: net.oa_key.as_deref(), mailto: net.mailto.as_deref() }
}

async fn fetch(net: &Net, ids: &[String]) -> Result<Vec<Paper>, String> {
    let url = openalex::ids_url(ids, &auth(net));
    let body = net.get_text(&url, Ttl::Api).await?.ok_or_else(|| "OpenAlex returned 404".to_string())?;
    Ok(openalex::parse(&body, ids)?.0)
}

fn note_truncation(net: &Net, papers: &[Paper]) {
    let n = papers.iter().filter(|p| p.partial & 2 != 0).count();
    if n > 0 {
        net.degrade(format!("{n} paper(s) have truncated author lists (OpenAlex returns at most 100 authors): author counts, \"+K\" and --au/--nau are unreliable for them"));
    }
}

/// Metadata for arXiv ids from OpenAlex (batched, 50 per request).
pub async fn ids(net: &Net, ids: &[String]) -> Result<Vec<Paper>, String> {
    let mut out = Vec::new();
    for chunk in ids.chunks(OA_IDS_BATCH) {
        out.extend(fetch(net, chunk).await?);
    }
    net.degrade("metadata via OpenAlex (arXiv API unavailable): categories and author comments are missing");
    note_truncation(net, &out);
    Ok(out)
}

/// Like `ids` but silent and best-effort (used to *enrich*, not to replace, arXiv metadata).
pub async fn ids_quiet(net: &Net, ids: &[String]) -> Result<Vec<Paper>, String> {
    let mut out = Vec::new();
    for chunk in ids.chunks(OA_IDS_BATCH) {
        out.extend(fetch(net, chunk).await?);
    }
    Ok(out)
}

/// Query constraints OpenAlex cannot express.
fn unsupported(a: &Args) -> Option<&'static str> {
    const FLAGS: &[(&str, &str)] = &[
        ("cat", "--cat"),
        ("xcat", "--xcat"),
        ("tag", "--tag"),
        ("co", "--co"),
        ("jr", "--jr"),
        ("pcat", "--pcat"),
        ("upd", "--upd"),
        ("id", "--id"),
        // field-scoped text and arXiv-only metadata cannot be evaluated from OpenAlex records
        ("ti", "--ti"),
        ("abs", "--abs"),
        ("cmt", "--cmt"),
        ("pages", "--pages"),
        ("preprint", "--preprint"),
        ("accepted", "--accepted"),
    ];
    FLAGS.iter().find(|(k, _)| a.flag(k)).map(|(_, f)| *f)
}

/// One term as OpenAlex expects it: phrases quoted exactly once (arXiv wildcards are not expressible).
fn quote(k: &str) -> Result<String, String> {
    let k = k.trim().trim_matches('"');
    no_wildcards(k)?;
    Ok(if k.contains(' ') { format!("\"{k}\"") } else { k.to_string() })
}

fn no_wildcards(text: &str) -> Result<(), String> {
    if text.contains(['*', '?']) {
        return Err("OpenAlex fallback cannot evaluate arXiv wildcards (* ?)".into());
    }
    Ok(())
}

/// The arXiv-style expression builder's output ("all:a AND all:\"b c\" ANDNOT all:d") as OpenAlex search
/// syntax: `all:` dropped, phrases kept, `ANDNOT` -> `AND NOT`; any other field prefix is unsupported.
fn oa_expr(text: &str) -> Result<String, String> {
    crate::query::check_negation(text)?;
    no_wildcards(text)?;
    let e = crate::query::expr("all", text);
    let mut toks: Vec<String> = Vec::new();
    let (mut cur, mut inq) = (String::new(), false);
    for c in e.chars().chain(std::iter::once(' ')) {
        match c {
            '"' => {
                inq = !inq;
                cur.push(c);
            }
            ' ' if !inq => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    let mut out: Vec<String> = Vec::new();
    for t in toks {
        if t == "ANDNOT" {
            out.push("AND NOT".into());
        } else if let Some(rest) = t.strip_prefix("all:") {
            out.push(rest.to_string());
        } else if t.contains(':') && !t.starts_with('"') {
            return Err("OpenAlex fallback cannot evaluate arXiv field prefixes (ti:, au:, cat:, ...)".into());
        } else {
            out.push(t);
        }
    }
    Ok(out.join(" "))
}

/// Free text for OpenAlex's `search=`: the same meaning as on the arXiv path (words ANDed, quoted phrases,
/// AND/OR/NOT/ANDNOT and parentheses); each flag's expression stays one parenthesised unit and exclusions
/// apply to the whole.
fn text_of(a: &Args) -> Result<Option<String>, String> {
    let mut units: Vec<String> = Vec::new();
    let group = |e: String| if e.contains(" OR ") { format!("({e})") } else { e };
    let plain = a.pos.join(" ");
    if !plain.trim().is_empty() {
        let e = oa_expr(&plain)?;
        if !e.is_empty() {
            units.push(group(e));
        }
    }
    for v in a.all("all") {
        let e = oa_expr(v)?;
        if !e.is_empty() {
            units.push(group(e));
        }
    }
    for g in a.groups("kw") {
        units.push(format!("({})", g.iter().map(|k| quote(k)).collect::<Result<Vec<_>, _>>()?.join(" OR ")));
    }
    let nots = a.groups("not").into_iter().flatten().map(|k| quote(&k)).collect::<Result<Vec<String>, _>>()?;
    if units.is_empty() {
        return if nots.is_empty() { Ok(None) } else { Err("OpenAlex fallback: --not needs a positive search term".into()) };
    }
    let mut t = units.join(" AND ");
    for n in nots {
        t.push_str(&format!(" AND NOT {n}"));
    }
    Ok(Some(t))
}

/// Whether the fallback could answer this search at all (checked before any request).
pub fn can_search(a: &Args) -> Result<(), String> {
    if let Some(f) = unsupported(a) {
        return Err(format!("OpenAlex fallback cannot apply {f}"));
    }
    text_of(a).map(|_| ())
}

/// Does the paper's arXiv month (from its id) overlap the `since`/`until` day bounds? Month precision is
/// all an OpenAlex-sourced paper has, so a month counts when any of its days could be inside.
fn month_in_range(published: &str, since: Option<&str>, until: Option<&str>) -> bool {
    let Some(ym) = published.get(..7) else { return true };
    let (y, m): (i64, u32) = match (ym[..4].parse(), ym[5..].parse()) {
        (Ok(y), Ok(m)) => (y, m),
        _ => return true,
    };
    let last = format!("{ym}-{:02}", dates::days_in_month(y, m));
    since.map_or(true, |s| last.as_str() >= s) && until.map_or(true, |u| format!("{ym}-01").as_str() <= u)
}

/// Search through OpenAlex with the client-side filters applied afterwards. Text and authors are expressed
/// natively; dates are judged by the arXiv id month (the semantics of every other path), so both bounds and
/// date ordering are applied locally over the scanned window (OpenAlex's publication date can be years off).
pub async fn search(net: &Net, a: &Args) -> Result<Found, String> {
    can_search(a)?;
    let filter = Filter::from_args(a)?;
    let n = a.num::<usize>("n").unwrap_or(10).clamp(1, crate::engine::MAX_ROWS);
    let skip = a.num::<usize>("skip").unwrap_or(0);
    if skip > crate::engine::MAX_ROWS {
        return Err(format!("--skip too large (max {})", crate::engine::MAX_ROWS));
    }
    if skip >= OA_MAX_WINDOW {
        return Err(format!("OpenAlex fallback cannot page past {OA_MAX_WINDOW} results (--skip {skip})"));
    }
    let want = n + skip;
    // OpenAlex basic paging ends at 10 000 results: cap the window instead of failing mid-scan
    let scan = a.num::<usize>("scan").unwrap_or(400).min(OA_MAX_WINDOW).max(want.min(OA_MAX_WINDOW));
    let text = text_of(a)?;
    // AND of groups, alternatives inside a group (OR) are kept
    // server side: the surname word only (OpenAlex matches whole words, so "F. Schlatt" would miss
    // "Ferdinand Schlatt"); the fuzzy author filter then decides locally
    let authors: Vec<Vec<String>> = a
        .groups("au")
        .into_iter()
        .map(|g| g.iter().map(|n| crate::fuzzy::AuthorQ::new(n).api_token()).filter(|t| !t.is_empty()).collect::<Vec<_>>())
        .filter(|g| !g.is_empty())
        .collect();
    let now = dates::today();
    let fmt = |d: dates::Ymd| format!("{:04}-{:02}-{:02}", d.0, d.1, d.2);
    let mut since = None;
    for s in a.all("since") {
        let d = dates::parse_bound(s, false, now)?;
        since = Some(since.map_or(d, |c: dates::Ymd| c.max(d)));
    }
    let mut until = None;
    for s in a.all("until") {
        let d = dates::parse_bound(s, true, now)?;
        until = Some(until.map_or(d, |c: dates::Ymd| c.min(d)));
    }
    let (since, until) = (since.map(fmt), until.map(fmt));
    if since.as_deref().zip(until.as_deref()).is_some_and(|(s, u)| s > u) {
        return Err("--since is after --until".into());
    }
    let sort = crate::query::parse_sort(a.get("sort").unwrap_or(if text.is_some() { "rel" } else { "new" }))?;
    if text.is_none() && authors.is_empty() && since.is_none() && until.is_none() {
        return Err("OpenAlex fallback needs text, an author or a date range".into());
    }
    if sort.0 == "lastUpdatedDate" {
        return Err("OpenAlex fallback cannot sort by arXiv update date (--sort upd/uold)".into());
    }
    let by_date = sort.0 != "relevance";
    let mut matches: Vec<Paper> = Vec::new();
    let (mut page, mut scanned) = (1usize, 0usize);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        let q = openalex::Query {
            text: text.as_deref(),
            authors: &authors,
            since: None,
            until: None,
            order: match (sort.0, sort.1) {
                ("relevance", _) => openalex::Order::Relevance,
                (_, "ascending") => openalex::Order::OldestFirst,
                _ => openalex::Order::NewestFirst,
            },
            per_page: OA_PAGE,
            page,
        };
        let body = net.get_text(&openalex::search_url(&q, &auth(net)), Ttl::Api).await?.ok_or_else(|| "OpenAlex returned 404".to_string())?;
        let pg = openalex::parse_page(&body, scan - scanned)?;
        scanned += pg.rows;
        let unmapped = pg.rows - pg.papers.len();
        if unmapped > 0 {
            net.degrade(format!("{unmapped} OpenAlex result(s) omitted: no arXiv id among their first 100 locations"));
        }
        let mut cands: Vec<Paper> = Vec::new();
        for mut p in pg.papers {
            // distinct OpenAlex works can map to one arXiv id
            if !seen.insert(p.id.clone()) {
                continue;
            }
            filter.check_provenance(&p)?;
            if month_in_range(&p.published, since.as_deref(), until.as_deref()) && filter.pass_meta(&mut p) {
                cands.push(p);
            }
        }
        crate::engine::enrich(net, &mut cands, filter.needs_insts(), filter.needs_msc()).await?;
        matches.extend(cands.into_iter().filter(|p| filter.pass_enriched(p)));
        // date-ordered requests need the whole window before the cut (the source orders by another date)
        let enough = matches.len() >= want && !by_date;
        if enough || pg.rows == 0 || scanned >= scan || scanned >= pg.total {
            break;
        }
        page += 1;
    }
    if by_date {
        crate::engine::sort_local(&mut matches, sort.0, sort.1);
    }
    net.degrade("results via OpenAlex (arXiv API unavailable): no categories/comments, dates are the arXiv id month, ordering covers only the scanned window, totals unknown");
    let papers: Vec<Paper> = matches.into_iter().skip(skip).take(n).collect();
    note_truncation(net, &papers);
    Ok(Found { papers, total: None, scan_to: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &[&str]) -> Args {
        crate::args::parse(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[tokio::test]
    async fn reversed_date_bounds_are_rejected_before_any_request() {
        let d = std::env::temp_dir().join(format!("arx-rev-{}", std::process::id()));
        let net = Net::for_test(&d, &["--via", "oa"]);
        let e = search(&net, &a(&["graph", "--since", "2024-03-31", "--until", "2024-03-01"])).await.err().unwrap();
        assert!(e.contains("--since is after --until"), "{e}");
    }

    #[tokio::test]
    async fn update_sorting_is_refused_instead_of_silently_becoming_submission_sorting() {
        let d = std::env::temp_dir().join(format!("arx-upd-{}", std::process::id()));
        let net = Net::for_test(&d, &["--via", "oa"]);
        let e = search(&net, &a(&["graph", "--sort", "upd"])).await.err().unwrap();
        assert!(e.contains("update date"), "{e}");
    }

    #[test]
    fn truncated_author_lists_are_noted() {
        let d = std::env::temp_dir().join(format!("arx-trunc-{}", std::process::id()));
        let net = Net::for_test(&d, &[]);
        let mut p = Paper::default();
        note_truncation(&net, std::slice::from_ref(&p));
        assert!(net.take_notes().is_empty());
        p.partial = 2;
        note_truncation(&net, &[p]);
        assert!(net.take_notes().iter().any(|n| n.contains("truncated author lists")));
    }

    #[test]
    fn month_range_semantics() {
        assert!(month_in_range("2024-03-01T00:00:00Z", Some("2024-03-15"), None)); // month overlaps the bound
        assert!(!month_in_range("2024-02-01T00:00:00Z", Some("2024-03-01"), None));
        assert!(month_in_range("2024-03-01T00:00:00Z", None, Some("2024-03-02")));
        assert!(!month_in_range("2024-04-01T00:00:00Z", None, Some("2024-03-31")));
        assert!(month_in_range("junk", Some("2024-01-01"), None));
    }

    #[test]
    fn expressible_queries() {
        assert!(can_search(&a(&["diffusion", "models", "-a", "tao", "--since", "2024"])).is_ok());
        assert_eq!(text_of(&a(&["graph", "--kw", "ramsey,\"turan number\"", "--not", "survey"])).unwrap().unwrap(), "graph AND (ramsey OR \"turan number\") AND NOT survey");
        assert_eq!(text_of(&a(&["--all", "quantum OR cosmology", "--all", "gravity"])).unwrap().unwrap(), "(quantum OR cosmology) AND gravity");
        assert_eq!(text_of(&a(&["diffusion ANDNOT survey"])).unwrap().unwrap(), "diffusion AND NOT survey");
        assert_eq!(text_of(&a(&["diffusion and models"])).unwrap().unwrap(), "diffusion AND models");
        assert_eq!(text_of(&a(&["--all", "\"diffusion models\""])).unwrap().unwrap(), "\"diffusion models\"");
        assert_eq!(text_of(&a(&["diffusion models"])).unwrap().unwrap(), "diffusion AND models");
        assert!(text_of(&a(&["quant*"])).is_err());
        assert!(text_of(&a(&["--kw", "quant*"])).is_err());
        assert!(text_of(&a(&["graph", "--not", "survey?"])).is_err());
        for q in ["NOT diffusion", "diffusion AND NOT", "a OR NOT b"] {
            assert!(text_of(&a(&[q])).is_err(), "{q}");
        }
        assert_eq!(text_of(&a(&["diffusion", "models"])).unwrap().unwrap(), "diffusion AND models");
        assert!(text_of(&a(&["--not", "survey"])).is_err());
        assert_eq!(text_of(&a(&["-a", "tao"])).unwrap(), None);
    }

    #[test]
    fn inexpressible_queries_say_why() {
        for (args, flag) in [(&["x", "-c", "math.CO"][..], "--cat"), (&["x", "--tag", "llm"], "--tag"), (&["x", "--xcat", "cs.*"], "--xcat"), (&["x", "--jr", "annals"], "--jr")] {
            let e = can_search(&a(args)).unwrap_err();
            assert!(e.contains(flag), "{e}");
        }
        assert!(can_search(&a(&["ti:olympiad"])).unwrap_err().contains("field prefixes"));
    }
}
