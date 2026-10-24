//! Search engine: paging, client-side filtering, concurrent enrichment, id batch fetch.

use futures_util::future::join_all;
use std::collections::HashSet;

use crate::args::Args;
use crate::filter::Filter;
use crate::model::Paper;
use crate::net::{Net, Ttl};
use crate::{atom, html, pstore, query};

pub struct Found {
    /// Date (YYYY-MM-DD, by the sort key) of the last raw row examined when the scan limit stopped the
    /// search before `n` matches were found; resumable consumers (alerts) advance their cursor to it.
    pub scan_to: Option<String>,
    pub papers: Vec<Paper>,
    /// Server total, only when no client-side filtering distorts it.
    pub total: Option<usize>,
}

const ID_CHUNK: usize = 100;

fn page_params(b: &query::Built, ids: Option<&[String]>, start: usize, max: usize) -> Vec<(&'static str, String)> {
    let mut p: Vec<(&'static str, String)> = Vec::with_capacity(6);
    if let Some(sq) = &b.sq {
        p.push(("search_query", sq.clone()));
    }
    if let Some(ids) = ids {
        p.push(("id_list", ids.join(",")));
    }
    p.push(("start", start.to_string()));
    p.push(("max_results", max.to_string()));
    if ids.is_none() {
        p.push(("sortBy", b.sort_by.to_string()));
        p.push(("sortOrder", b.order.to_string()));
    }
    p
}

/// Rows per API request. Pages are always fetched on fixed 100-row boundaries, so `-n 5`, `-n 50` and
/// `--skip 5` over the same query share one cached response (one API call, not three).
const CHUNK: usize = 100;

/// Rows `[start, start + max)` of a query through aligned `CHUNK`-row pages; every paper seen is also
/// remembered in the per-paper store. Returns (rows, server total).
async fn fetch_rows(net: &Net, built: &query::Built, start: usize, max: usize, memo: &mut Memo) -> Result<(Vec<Paper>, usize), String> {
    let chunk_start = start / CHUNK * CHUNK;
    // within one invocation a page is downloaded once, even under --fresh (small enrichment steps re-enter it)
    if !memo.contains_key(&chunk_start) {
        let (xml, stale) = net.api_ex(&page_params(built, None, chunk_start, CHUNK)).await?;
        let feed = atom::parse(&xml)?;
        if !stale {
            // an expired page must not renew its papers' freshness in the store
            for p in &feed.papers {
                pstore::put(net, p);
            }
        }
        memo.insert(chunk_start, (feed.papers, feed.total));
    }
    let (papers, total) = &memo[&chunk_start];
    Ok((papers.iter().skip(start - chunk_start).take(max).cloned().collect(), *total))
}

/// Pages already fetched by the current search pass, by aligned start offset.
type Memo = std::collections::HashMap<usize, (Vec<Paper>, usize)>;

/// Metadata for exact ids, order preserved, unknown ids skipped. Served from the per-paper store when
/// possible (no API call); the rest is fetched in batched `id_list` calls and stored. When arXiv is
/// unavailable, stale stored entries answer instead.
pub async fn fetch_ids(net: &Net, ids: &[String]) -> Result<Vec<Paper>, String> {
    let mut seen = HashSet::new();
    let ids: Vec<&String> = ids.iter().filter(|i| seen.insert((*i).clone())).collect();
    let mut have: std::collections::HashMap<String, Paper> = std::collections::HashMap::new();
    let mut missing: Vec<String> = Vec::new();
    for id in &ids {
        match pstore::get(net, id, true) {
            Some(p) => {
                have.insert((*id).clone(), p);
            }
            None => missing.push((*id).clone()),
        }
    }
    if !missing.is_empty() && net.via == crate::net::Via::Oa {
        // OpenAlex only: never ask arXiv
        have.extend(crate::fallback::ids(net, &missing).await?.into_iter().map(|p| (p.id.clone(), p)));
    } else if !missing.is_empty() {
        let mut fetched: Result<(), String> = Ok(());
        for chunk in missing.chunks(ID_CHUNK) {
            let params = vec![("id_list", chunk.join(",")), ("max_results", chunk.len().to_string())];
            let r = match net.api_ex(&params).await {
                Ok((xml, stale)) => atom::parse(&xml).map(|f| (f.papers, stale)),
                Err(e) => Err(e),
            };
            match r {
                Ok((papers, stale)) => {
                    for p in papers {
                        if !stale {
                            pstore::put(net, &p);
                        }
                        have.insert(p.id.clone(), p);
                    }
                }
                Err(e) => {
                    fetched = Err(e);
                    break;
                }
            }
        }
        if let Err(e) = fetched {
            if !crate::net::is_net_err(&e) {
                return Err(e);
            }
            // arXiv unavailable: stale store entries, then (only in auto mode) other sources
            let mut still: Vec<String> = Vec::new();
            for id in &missing {
                if have.contains_key(id) {
                    continue;
                }
                match pstore::get(net, id, false) {
                    Some(p) => {
                        net.degrade(format!("{id}: stale cached metadata (arXiv API unavailable)"));
                        have.insert(id.clone(), p);
                    }
                    None => still.push(id.clone()),
                }
            }
            if !still.is_empty() {
                let oa = if matches!(net.via, crate::net::Via::Auto | crate::net::Via::Cache) { crate::fallback::ids(net, &still).await } else { Err("other sources disabled by --via".to_string()) };
                match oa {
                    Ok(ps) => have.extend(ps.into_iter().map(|p| (p.id.clone(), p))),
                    Err(oe) => {
                        if have.is_empty() {
                            return Err(format!("{e}; fallback failed: {oe}"));
                        }
                        // partial answer: say which ids are missing instead of silently dropping them
                        net.degrade(format!("{} id(s) unavailable ({}): {}", still.len(), e.trim_start_matches(crate::net::NET_ERR).trim().chars().take(60).collect::<String>(), still.join(",")));
                    }
                }
            }
        }
    }
    Ok(ids.iter().filter_map(|i| have.remove(*i)).collect())
}

/// Fill insts (from HTML head) and/or msc (from abs page) concurrently. A failed fetch (anything but a
/// 404, which just means "no data") is an error: filters that depend on the data must not silently
/// treat an outage as "no match".
pub async fn enrich(net: &Net, papers: &mut [Paper], insts: bool, msc: bool) -> Result<(), String> {
    if !insts && !msc {
        return Ok(());
    }
    if msc && net.via == crate::net::Via::Oa {
        return Err("--msc / msc field need arxiv.org abs pages, which --via oa never fetches".into());
    }
    // institutions: one OpenAlex request covers 50 papers, so ask it first and scrape arxiv.org pages
    // only for the papers it does not know (or knows without affiliations)
    if insts && matches!(net.via, crate::net::Via::Auto | crate::net::Via::Oa | crate::net::Via::Cache) {
        let ids: Vec<String> = papers.iter().filter(|p| p.enriched & 1 == 0).map(|p| p.id.clone()).collect();
        if !ids.is_empty() {
            match crate::fallback::ids_quiet(net, &ids).await {
                Ok(found) => {
                    let by_id: std::collections::HashMap<&str, &Paper> = found.iter().map(|p| (p.id.as_str(), p)).collect();
                    for p in papers.iter_mut().filter(|p| p.enriched & 1 == 0) {
                        // only complete institution lists count (truncated authorships leave `enriched` unset)
                        if let Some(f) = by_id.get(p.id.as_str()) {
                            p.insts = f.insts.clone();
                            if f.enriched & 1 != 0 {
                                p.enriched |= 1;
                            }
                        }
                    }
                }
                // OpenAlex-only mode has nothing else to try: surface the outage instead of "no institutions"
                Err(e) if net.via == crate::net::Via::Oa => return Err(e),
                Err(_) => {}
            }
        }
        if net.via == crate::net::Via::Oa {
            let incomplete = papers.iter().filter(|p| p.enriched & 1 == 0).count();
            for p in papers.iter_mut() {
                p.enriched |= 1; // OpenAlex only: no page scraping for the rest
            }
            if incomplete > 0 {
                net.degrade(format!("institutions incomplete for {incomplete} paper(s): OpenAlex lacks or truncates their affiliations (--via oa never reads arxiv.org)"));
            }
        }
    }
    let futs = papers.iter().map(|p| {
        let id = p.id.clone();
        let (insts, msc) = (insts && p.enriched & 1 == 0, msc && p.enriched & 2 == 0);
        async move {
            let i = async {
                if !insts {
                    return Ok(Vec::new());
                }
                for base in ["https://arxiv.org/html/", "https://ar5iv.labs.arxiv.org/html/"] {
                    if let Some(h) = net.get_text(&format!("{base}{id}"), Ttl::Doc).await? {
                        return Ok(html::insts_only(&h));
                    }
                }
                Ok(Vec::new())
            };
            let m = async {
                if !msc {
                    return Ok(Vec::new());
                }
                match net.get_text(&format!("https://arxiv.org/abs/{id}"), Ttl::Doc).await? {
                    Some(h) => Ok(html::abs_meta(&h).msc),
                    None => Ok(Vec::new()),
                }
            };
            futures_util::join!(i, m)
        }
    });
    let res: Vec<(Result<Vec<String>, String>, Result<Vec<String>, String>)> = join_all(futs).await;
    // papers whose institutions could not be read because arxiv.org throttled: ask OpenAlex instead
    let mut via_oa: Vec<usize> = Vec::new();
    let mut first_err: Option<String> = None;
    for (idx, (p, (i, m))) in papers.iter_mut().zip(res).enumerate() {
        let m = m?;
        if insts && p.enriched & 1 == 0 {
            match i {
                Ok(v) => {
                    if v.is_empty() && !p.insts.is_empty() {
                        net.degrade(format!("{}: institutions from OpenAlex may be incomplete (arxiv.org page lists none)", p.id));
                    }
                    // keep what OpenAlex already gave (its list may be partial); add what the page shows
                    for x in v {
                        if !p.insts.contains(&x) {
                            p.insts.push(x);
                        }
                    }
                    p.enriched |= 1;
                }
                Err(e) if crate::net::is_net_err(&e) && matches!(net.via, crate::net::Via::Auto | crate::net::Via::Cache) => {
                    first_err.get_or_insert(e);
                    via_oa.push(idx);
                }
                Err(e) => return Err(e),
            }
        }
        if msc && p.enriched & 2 == 0 {
            p.msc = m;
            p.enriched |= 2;
        }
    }
    if !via_oa.is_empty() {
        let ids: Vec<String> = via_oa.iter().map(|&k| papers[k].id.clone()).collect();
        let Ok(found) = crate::fallback::ids(net, &ids).await else {
            return Err(first_err.unwrap_or_default());
        };
        let by_id: std::collections::HashMap<&str, &Paper> = found.iter().map(|p| (p.id.as_str(), p)).collect();
        let mut missing = 0;
        for k in via_oa {
            match by_id.get(papers[k].id.as_str()) {
                Some(f) => {
                    papers[k].insts = f.insts.clone();
                    if f.enriched & 1 == 0 {
                        net.degrade(format!("{}: institutions from OpenAlex may be incomplete", papers[k].id));
                    }
                }
                None => missing += 1,
            }
            papers[k].enriched |= 1;
        }
        net.degrade("institutions via OpenAlex (arxiv.org throttled)");
        if missing > 0 {
            net.degrade(format!("institutions unavailable for {missing} paper(s): not in OpenAlex either"));
        }
    }
    Ok(())
}

/// Stable local ordering by an arXiv `sortBy`/`sortOrder` pair (relevance = keep order).
pub fn sort_local(papers: &mut [Paper], sort_by: &str, order: &str) {
    match sort_by {
        "submittedDate" => papers.sort_by(|x, y| x.published.cmp(&y.published)),
        "lastUpdatedDate" => papers.sort_by(|x, y| x.updated.cmp(&y.updated)),
        _ => return,
    }
    if order == "descending" {
        papers.reverse();
    }
}

/// arXiv never serves more than this many rows per query.
pub const MAX_ROWS: usize = 30_000;

/// Run a search as described by `a`. arXiv first (cache, aligned pages, stale entries on outage); when it
/// is unavailable and the query is expressible, OpenAlex answers instead (`--via oa` forces that).
pub async fn search(net: &Net, a: &Args) -> Result<Found, String> {
    use crate::net::Via;
    if net.via == Via::Oa {
        return crate::fallback::search(net, a).await;
    }
    match search_arxiv(net, a).await {
        Err(e) if crate::net::is_net_err(&e) && matches!(net.via, Via::Auto | Via::Cache) => match crate::fallback::search(net, a).await {
            Ok(f) => Ok(f),
            Err(fe) => Err(format!("{e}; no fallback possible: {fe}")),
        },
        r => r,
    }
}

async fn search_arxiv(net: &Net, a: &Args) -> Result<Found, String> {
    let filter = Filter::from_args(a)?;
    let n = a.num::<usize>("n").unwrap_or(10).clamp(1, MAX_ROWS);
    let skip = a.num::<usize>("skip").unwrap_or(0);
    if skip > MAX_ROWS {
        return Err(format!("--skip too large (max {MAX_ROWS})"));
    }
    // without client-side filtering the API can skip rows for us
    let api_skip = if filter.any() || a.flag("id") { 0 } else { skip };
    let want = n + (skip - api_skip);
    let scan = a.num::<usize>("scan").unwrap_or(400).min(MAX_ROWS).max(want);
    let (need_i, need_m) = (filter.needs_insts(), filter.needs_msc());
    let has_au = a.flag("au");

    let mut seen: HashSet<String> = HashSet::new();
    let mut matches: Vec<Paper> = Vec::new();
    let mut server_total: Option<usize> = None;
    let mut relaxed_sort: Option<(&str, &str)> = None;
    let mut scan_to: Option<String> = None;

    // explicit ordering (and cursors built on it, like alerts) needs completeness: a given-name AND query
    // would miss "T. Tao" papers, so go straight to the surname-only pass
    let mut relax = u8::from(has_au && a.flag("sort"));
    loop {
        let built = query::build(a, relax)?;
        if built.sq.is_none() && built.ids.is_empty() {
            return Err("empty query: give text, --au/--cat/--tag/--since/--id ... (see `arx -h`)".into());
        }
        let id_mode = !built.ids.is_empty();
        let mut memo = Memo::new();
        let (mut start, mut cursor, mut scanned) = (api_skip, 0usize, 0usize);
        loop {
            let (slice, max): (Option<&[String]>, usize) = if id_mode {
                let end = (cursor + ID_CHUNK).min(built.ids.len());
                (Some(&built.ids[cursor..end]), end - cursor)
            } else {
                // enrichment costs one fetch per paper: take small steps so the search stops as soon as it has enough
                let page = if need_i || need_m { 40 } else if filter.any() { 200 } else { want };
                (None, page.min(scan.saturating_sub(scanned)).clamp(1, 2000))
            };
            let (rows, total) = if id_mode {
                // ids intersected with query constraints: the API must answer (the store cannot)
                let (xml, stale) = net.api_ex(&page_params(&built, slice, start, max)).await?;
                let feed = atom::parse(&xml)?;
                if !stale {
                    for p in &feed.papers {
                        pstore::put(net, p);
                    }
                }
                (feed.papers, feed.total)
            } else {
                fetch_rows(net, &built, start, max, &mut memo).await?
            };
            let got = rows.len();
            server_total.get_or_insert(total);
            let page_last = rows.last().map(|p| if built.sort_by == "lastUpdatedDate" { p.updated_date().to_string() } else { p.date().to_string() });
            if id_mode {
                cursor += max;
            } else {
                scanned += got;
                start += got;
            }
            let mut cands: Vec<Paper> = Vec::new();
            for mut p in rows {
                if !seen.contains(&p.id) && filter.pass_meta(&mut p) {
                    cands.push(p);
                }
            }
            enrich(net, &mut cands, need_i, need_m).await?;
            for p in cands {
                if filter.pass_enriched(&p) && seen.insert(p.id.clone()) {
                    matches.push(p);
                }
            }
            if !id_mode && scanned >= scan && start < total {
                scan_to = page_last;
            }
            let exhausted = if id_mode { cursor >= built.ids.len() } else { got == 0 || start >= total || scanned >= scan };
            // an explicit --sort over ids needs every chunk before the cut
            let need_all = id_mode && a.flag("sort");
            if (matches.len() >= want && !need_all) || exhausted {
                break;
            }
        }
        if matches.len() >= want || !has_au {
            break;
        }
        relax = match relax {
            0 if built.has_strict_au => 1,
            0 | 1 if matches.is_empty() => 2,
            _ => break,
        };
        relaxed_sort = Some((built.sort_by, built.order));
        if relax == 2 && !matches.is_empty() {
            break;
        }
    }
    // passes were appended in pass order and id lookups carry no server ordering: apply the requested
    // ordering locally, before truncation
    if let Some((sort_by, order)) = relaxed_sort {
        sort_local(&mut matches, sort_by, order);
    }
    if a.flag("id") {
        if let Some(s) = a.get("sort") {
            let (sort_by, order) = query::parse_sort(s)?;
            sort_local(&mut matches, sort_by, order);
        }
    }
    let total = if filter.any() || a.flag("id") { None } else { server_total };
    let papers: Vec<Paper> = matches.into_iter().skip(skip - api_skip).take(n).collect();
    let scan_to = if papers.len() < n { scan_to } else { None };
    Ok(Found { papers, total, scan_to })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("arx-eng-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn paper(id: &str, title: &str) -> Paper {
        Paper { id: id.into(), title: title.into(), published: "2024-01-01T00:00:00Z".into(), ..Default::default() }
    }

    #[tokio::test]
    async fn msc_enrichment_is_refused_when_only_openalex_is_allowed() {
        let d = tmp("mscoa");
        let n = Net::for_test(&d, &["--via", "oa"]);
        let mut ps = vec![paper("2401.00001", "t")];
        let e = enrich(&n, &mut ps, false, true).await.unwrap_err();
        assert!(e.contains("--via oa"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn fresh_store_entries_need_no_api_call() {
        let d = tmp("fresh");
        let n = Net::for_test(&d, &["--via", "cache"]);
        pstore::put(&n, &paper("2401.00001", "Stored"));
        pstore::put(&n, &paper("2401.00002", "Also stored"));
        let got = fetch_ids(&n, &["2401.00002".into(), "2401.00001".into(), "2401.00002".into()]).await.unwrap();
        assert_eq!(got.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["2401.00002", "2401.00001"]);
        assert!(n.take_notes().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn stale_entries_answer_when_the_api_is_unavailable() {
        let d = tmp("stale");
        let n = Net::for_test(&d, &["--via", "cache"]);
        pstore::put(&n, &paper("2401.00001", "Stored long ago"));
        // age the entry past the freshness window
        let f = n.cache_file("arx-meta:2401.00001").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * pstore::FRESH_SECS);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
        let got = fetch_ids(&n, &["2401.00001".into(), "2401.99999".into()]).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "Stored long ago");
        let notes = n.take_notes().join(" | ");
        assert!(notes.contains("stale cached metadata") && notes.contains("2401.99999"), "{notes}");
        // nothing stored at all: a clear error instead of an empty answer
        let e = fetch_ids(&n, &["2401.88888".into()]).await.unwrap_err();
        assert!(crate::net::is_net_err(&e), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sort_local_orders_and_is_stable() {
        let p = |id: &str, d: &str| Paper { id: id.into(), published: d.into(), updated: d.into(), ..Default::default() };
        let mut v = vec![p("b", "2020"), p("a", "2018"), p("c", "2022")];
        sort_local(&mut v, "submittedDate", "descending");
        assert_eq!(v.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["c", "b", "a"]);
        sort_local(&mut v, "submittedDate", "ascending");
        assert_eq!(v.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        sort_local(&mut v, "relevance", "descending");
        assert_eq!(v[0].id, "a");
    }

    #[test]
    fn params_include_sort_only_for_queries() {
        let b = query::Built { sq: Some("all:x".into()), ids: vec![], sort_by: "relevance", order: "descending", has_strict_au: false };
        let p = page_params(&b, None, 5, 10);
        assert!(p.iter().any(|(k, v)| *k == "sortBy" && v == "relevance"));
        let ids = vec!["1".to_string()];
        let p = page_params(&b, Some(&ids), 0, 1);
        assert!(!p.iter().any(|(k, _)| *k == "sortBy"));
    }
}
