//! Per-paper metadata store on top of the disk cache: every paper parsed from any API response is kept
//! under its id, so `get`, `related`, piped hydration and repeated lookups need no API call at all, and
//! stale entries can answer when arXiv is throttling.

use crate::model::Paper;
use crate::net::Net;
use serde_json::{json, Value};

/// Entries older than this are refetched (unless the API is unavailable: then any age is served).
pub const FRESH_SECS: u64 = 24 * 3600;

fn key(id: &str) -> String {
    format!("arx-meta:{id}")
}

pub fn to_json(p: &Paper) -> String {
    json!({
        "id": p.id, "ver": p.ver, "title": p.title, "authors": p.authors, "affs": p.affs, "abs": p.abs,
        "cats": p.cats, "published": p.published, "updated": p.updated, "comment": p.comment, "jref": p.jref,
        "doi": p.doi, "msc": p.msc, "insts": p.insts, "enriched": p.enriched,
    })
    .to_string()
}

pub fn from_json(s: &str) -> Option<Paper> {
    let v: Value = serde_json::from_str(s).ok()?;
    let strs = |k: &str| -> Vec<String> { v[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
    let opt = |k: &str| v[k].as_str().map(String::from);
    let id = opt("id").filter(|i| !i.is_empty())?;
    Some(Paper {
        id,
        ver: v["ver"].as_u64().unwrap_or(0) as u32,
        title: opt("title").unwrap_or_default(),
        authors: strs("authors"),
        affs: strs("affs"),
        abs: opt("abs").unwrap_or_default(),
        cats: strs("cats"),
        published: opt("published").unwrap_or_default(),
        updated: opt("updated").unwrap_or_default(),
        comment: opt("comment"),
        jref: opt("jref"),
        doi: opt("doi"),
        msc: strs("msc"),
        venue: None,
        insts: strs("insts"),
        enriched: v["enriched"].as_u64().unwrap_or(0) as u8,
        partial: 0,
    })
}

/// `fresh_only`: honour the 24 h freshness window and `--fresh`; otherwise any age is accepted (stale
/// fallback while the API is unavailable).
pub fn get(net: &Net, id: &str, fresh_only: bool) -> Option<Paper> {
    let body = net.cache_read(&key(id), fresh_only.then(|| net.store_ttl(FRESH_SECS)), fresh_only)??;
    from_json(&String::from_utf8_lossy(&body))
}

/// Remember a paper. An older observation never replaces a newer stored one (a cached search page that
/// still shows v1 must not overwrite the v2 record a later `get` fetched).
pub fn put(net: &Net, p: &Paper) {
    // read-compare-write under one cross-process lock: two processes cannot interleave and let v1 win
    net.with_lock("pstore", || {
        if let Some(have) = get(net, &p.id, false) {
            if have.ver > p.ver || (have.ver == p.ver && have.updated > p.updated) {
                return;
            }
        }
        net.cache_put(&key(&p.id), Some(to_json(p).as_bytes()));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_every_stored_field() {
        let p = Paper {
            id: "math/0309285".into(),
            ver: 3,
            title: "T \"quoted\" é".into(),
            authors: vec!["A B".into(), "C D".into()],
            affs: vec!["MIT".into()],
            abs: "x".into(),
            cats: vec!["math.CO".into(), "math.NT".into()],
            published: "2003-09-01T00:00:00Z".into(),
            updated: "2004-01-01T00:00:00Z".into(),
            comment: Some("12 pages".into()),
            jref: Some("Ann. Math. 1 (2000)".into()),
            doi: None,
            msc: vec!["05C10".into()],
            venue: None,
            insts: vec!["Oxford".into()],
            enriched: 3,
            partial: 0,
        };
        let q = from_json(&to_json(&p)).unwrap();
        assert_eq!(format!("{p:?}"), format!("{q:?}"));
    }

    #[test]
    fn an_older_observation_never_replaces_a_newer_one() {
        let d = std::env::temp_dir().join(format!("arx-pstore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let n = Net::for_test(&d, &["--via", "cache"]);
        let mk = |ver: u32, upd: &str, title: &str| Paper { id: "2401.00001".into(), ver, updated: upd.into(), title: title.into(), ..Default::default() };
        put(&n, &mk(1, "2024-01-01", "v1"));
        put(&n, &mk(2, "2024-02-01", "v2"));
        put(&n, &mk(1, "2024-01-01", "v1 again"));
        assert_eq!(get(&n, "2401.00001", false).unwrap().title, "v2");
        put(&n, &mk(2, "2024-03-01", "v2 edited"));
        assert_eq!(get(&n, "2401.00001", false).unwrap().title, "v2 edited");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn explicit_ttl_shortens_store_freshness() {
        let d = std::env::temp_dir().join(format!("arx-pstore-ttl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let strict = Net::for_test(&d, &["--ttl", "1"]);
        let default = Net::for_test(&d, &[]);
        put(&default, &Paper { id: "2401.00001".into(), ver: 1, ..Default::default() });
        let f = default.cache_file(&key("2401.00001")).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
        assert!(get(&default, "2401.00001", true).is_some());
        assert!(get(&strict, "2401.00001", true).is_none());
        assert!(get(&strict, "2401.00001", false).is_some(), "stale reads ignore the ttl");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn junk_is_rejected() {
        assert!(from_json("").is_none());
        assert!(from_json("{}").is_none());
        assert!(from_json("{\"id\":\"\"}").is_none());
        assert!(from_json("[1,2]").is_none());
    }
}
