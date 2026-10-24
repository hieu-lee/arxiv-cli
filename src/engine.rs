//! Search engine: paging, client-side filtering, concurrent enrichment, id batch fetch.

use futures_util::future::join_all;
use std::collections::HashSet;

use crate::args::Args;
use crate::filter::Filter;
use crate::model::Paper;
use crate::net::{Net, Ttl};
use crate::{atom, html, query};

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

/// Fetch metadata for exact ids (chunked, order preserved, missing ids skipped). Unfiltered.
pub async fn fetch_ids(net: &Net, ids: &[String]) -> Result<Vec<Paper>, String> {
    let mut by_id: std::collections::HashMap<String, Paper> = std::collections::HashMap::new();
    for chunk in ids.chunks(ID_CHUNK) {
        let params = vec![("id_list", chunk.join(",")), ("max_results", chunk.len().to_string())];
        let feed = atom::parse(&net.api(&params).await?)?;
        for p in feed.papers {
            by_id.insert(p.id.clone(), p);
        }
    }
    let mut seen = HashSet::new();
    Ok(ids.iter().filter(|i| seen.insert((*i).clone())).filter_map(|i| by_id.remove(i)).collect())
}

/// Fill insts (from HTML head) and/or msc (from abs page) concurrently. A failed fetch (anything but a
/// 404, which just means "no data") is an error: filters that depend on the data must not silently
/// treat an outage as "no match".
pub async fn enrich(net: &Net, papers: &mut [Paper], insts: bool, msc: bool) -> Result<(), String> {
    if !insts && !msc {
        return Ok(());
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
    for (p, (i, m)) in papers.iter_mut().zip(res) {
        let (i, m) = (i?, m?);
        if insts && p.enriched & 1 == 0 {
            p.insts = i;
            p.enriched |= 1;
        }
        if msc && p.enriched & 2 == 0 {
            p.msc = m;
            p.enriched |= 2;
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

/// Run a search as described by `a` (query flags, filters, paging).
pub async fn search(net: &Net, a: &Args) -> Result<Found, String> {
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
        let (mut start, mut cursor, mut scanned) = (api_skip, 0usize, 0usize);
        loop {
            let (slice, max): (Option<&[String]>, usize) = if id_mode {
                let end = (cursor + ID_CHUNK).min(built.ids.len());
                (Some(&built.ids[cursor..end]), end - cursor)
            } else {
                let page = if filter.any() { 200 } else { want };
                (None, page.min(scan.saturating_sub(scanned)).clamp(1, 2000))
            };
            let params = page_params(&built, slice, start, max);
            let feed = atom::parse(&net.api(&params).await?)?;
            let got = feed.papers.len();
            server_total.get_or_insert(feed.total);
            let page_last = feed.papers.last().map(|p| if built.sort_by == "lastUpdatedDate" { p.updated_date().to_string() } else { p.date().to_string() });
            if id_mode {
                cursor += max;
            } else {
                scanned += got;
                start += got;
            }
            let mut cands: Vec<Paper> = Vec::new();
            for mut p in feed.papers {
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
            if !id_mode && scanned >= scan && start < feed.total {
                scan_to = page_last;
            }
            let exhausted = if id_mode { cursor >= built.ids.len() } else { got == 0 || got < max || start >= feed.total || scanned >= scan };
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
