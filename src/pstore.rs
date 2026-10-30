//! Per-paper metadata store on top of the disk cache: every paper parsed from any API response is kept
//! under its id, so `get`, `related`, piped hydration and repeated lookups need no API call at all, and
//! stale entries can answer when arXiv is throttling.

use crate::model::Paper;
use crate::net::Net;
use serde_json::Value;
use std::fmt::Write;

/// Entries older than this are refetched (unless the API is unavailable: then any age is served).
pub const FRESH_SECS: u64 = 24 * 3600;

fn key(id: &str) -> String {
    format!("arx-meta:{id}")
}

fn push_str(o: &mut String, s: &str) {
    o.push('"');
    let mut plain = 0;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        let esc = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x8 => "\\b",
            0xc => "\\f",
            0..=0x1f => {
                o.push_str(&s[plain..i]);
                plain = i + 1;
                let _ = write!(o, "\\u{b:04x}");
                continue;
            }
            _ => continue,
        };
        o.push_str(&s[plain..i]);
        plain = i + 1;
        o.push_str(esc);
    }
    o.push_str(&s[plain..]);
    o.push('"');
}

fn push_strs(o: &mut String, v: &[String]) {
    o.push('[');
    for (i, s) in v.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        push_str(o, s);
    }
    o.push(']');
}

fn push_opt(o: &mut String, s: &Option<String>) {
    match s {
        Some(s) => push_str(o, s),
        None => o.push_str("null"),
    }
}

/// Compact JSON with keys in sorted order (the layout `serde_json::Value` objects print in).
pub fn to_json(p: &Paper) -> String {
    let len = p.abs.len() + p.title.len() + p.authors.iter().map(|a| a.len() + 3).sum::<usize>() + 256;
    let mut o = String::with_capacity(len);
    o.push_str("{\"abs\":");
    push_str(&mut o, &p.abs);
    o.push_str(",\"affs\":");
    push_strs(&mut o, &p.affs);
    o.push_str(",\"authors\":");
    push_strs(&mut o, &p.authors);
    o.push_str(",\"cats\":");
    push_strs(&mut o, &p.cats);
    o.push_str(",\"comment\":");
    push_opt(&mut o, &p.comment);
    o.push_str(",\"doi\":");
    push_opt(&mut o, &p.doi);
    let _ = write!(o, ",\"enriched\":{},\"id\":", p.enriched);
    push_str(&mut o, &p.id);
    o.push_str(",\"insts\":");
    push_strs(&mut o, &p.insts);
    o.push_str(",\"jref\":");
    push_opt(&mut o, &p.jref);
    o.push_str(",\"msc\":");
    push_strs(&mut o, &p.msc);
    o.push_str(",\"published\":");
    push_str(&mut o, &p.published);
    o.push_str(",\"title\":");
    push_str(&mut o, &p.title);
    o.push_str(",\"updated\":");
    push_str(&mut o, &p.updated);
    let _ = write!(o, ",\"ver\":{}}}", p.ver);
    o
}

/// Cursor over the exact layout `to_json` writes; any deviation gives `None` (the caller then takes the
/// general `serde_json` path), so this never accepts something the general path would read differently.
struct Canon<'a> {
    s: &'a str,
    at: usize,
}

impl<'a> Canon<'a> {
    fn lit(&mut self, t: &str) -> Option<()> {
        self.s[self.at..].starts_with(t).then(|| self.at += t.len())
    }
    fn str(&mut self) -> Option<String> {
        self.lit("\"")?;
        let b = self.s.as_bytes();
        let mut out = String::new();
        loop {
            let run = b[self.at..].iter().position(|&c| c == b'"' || c == b'\\' || c < 0x20)?;
            out.push_str(&self.s[self.at..self.at + run]);
            self.at += run;
            match b[self.at] {
                b'"' => {
                    self.at += 1;
                    return Some(out);
                }
                b'\\' => {
                    let e = *b.get(self.at + 1)?;
                    self.at += 2;
                    out.push(match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hex = self.s.get(self.at..self.at + 4)?;
                            self.at += 4;
                            char::from_u32(u32::from_str_radix(hex, 16).ok().filter(|_| hex.bytes().all(|c| c.is_ascii_hexdigit()))?)?
                        }
                        _ => return None,
                    });
                }
                _ => return None,
            }
        }
    }
    fn opt(&mut self) -> Option<Option<String>> {
        if self.lit("null").is_some() {
            Some(None)
        } else {
            self.str().map(Some)
        }
    }
    fn strs(&mut self) -> Option<Vec<String>> {
        self.lit("[")?;
        let mut v = Vec::new();
        if self.lit("]").is_some() {
            return Some(v);
        }
        loop {
            v.push(self.str()?);
            if self.lit("]").is_some() {
                return Some(v);
            }
            self.lit(",")?;
        }
    }
    fn num<T: std::str::FromStr>(&mut self) -> Option<T> {
        let n = self.s.as_bytes()[self.at..].iter().take_while(|c| c.is_ascii_digit()).count();
        if n > 1 && self.s.as_bytes()[self.at] == b'0' {
            return None;
        }
        let v = self.s[self.at..self.at + n].parse().ok()?;
        self.at += n;
        Some(v)
    }
}

fn from_canonical(s: &str) -> Option<Paper> {
    let mut c = Canon { s, at: 0 };
    c.lit("{\"abs\":")?;
    let abs = c.str()?;
    c.lit(",\"affs\":")?;
    let affs = c.strs()?;
    c.lit(",\"authors\":")?;
    let authors = c.strs()?;
    c.lit(",\"cats\":")?;
    let cats = c.strs()?;
    c.lit(",\"comment\":")?;
    let comment = c.opt()?;
    c.lit(",\"doi\":")?;
    let doi = c.opt()?;
    c.lit(",\"enriched\":")?;
    let enriched = c.num()?;
    c.lit(",\"id\":")?;
    let id = c.str().filter(|i| !i.is_empty())?;
    c.lit(",\"insts\":")?;
    let insts = c.strs()?;
    c.lit(",\"jref\":")?;
    let jref = c.opt()?;
    c.lit(",\"msc\":")?;
    let msc = c.strs()?;
    c.lit(",\"published\":")?;
    let published = c.str()?;
    c.lit(",\"title\":")?;
    let title = c.str()?;
    c.lit(",\"updated\":")?;
    let updated = c.str()?;
    c.lit(",\"ver\":")?;
    let ver = c.num()?;
    c.lit("}")?;
    (c.at == s.len()).then_some(Paper { id, ver, title, authors, affs, abs, cats, published, updated, comment, jref, doi, msc, venue: None, insts, enriched, partial: 0 })
}

pub fn from_json(s: &str) -> Option<Paper> {
    if let Some(p) = from_canonical(s) {
        return Some(p);
    }
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
#[cfg(test)]
pub fn put(net: &Net, p: &Paper) {
    put_all(net, std::slice::from_ref(p));
}

/// `put` for a whole response: one cross-process lock for the batch instead of one per paper.
pub fn put_all(net: &Net, ps: &[Paper]) {
    if ps.is_empty() {
        return;
    }
    // read-compare-write under one cross-process lock: two processes cannot interleave and let v1 win
    net.with_lock("pstore", || {
        for p in ps {
            if let Some(have) = get(net, &p.id, false) {
                if have.ver > p.ver || (have.ver == p.ver && have.updated > p.updated) {
                    continue;
                }
            }
            net.cache_put(&key(&p.id), Some(to_json(p).as_bytes()));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn json_layout_is_serde_compatible_and_reordered_input_still_loads() {
        let p = Paper {
            id: "2401.00001".into(),
            ver: 2,
            title: "ctl \u{1}\u{8}\u{c}\u{1f} \u{7f} \"q\" \\ / é \u{1F600}".into(),
            authors: vec!["A".into()],
            comment: Some(String::new()),
            enriched: 3,
            ..Default::default()
        };
        let want = json!({
            "id": p.id, "ver": p.ver, "title": p.title, "authors": p.authors, "affs": p.affs, "abs": p.abs,
            "cats": p.cats, "published": p.published, "updated": p.updated, "comment": p.comment, "jref": p.jref,
            "doi": p.doi, "msc": p.msc, "insts": p.insts, "enriched": p.enriched,
        })
        .to_string();
        assert_eq!(to_json(&p), want);
        let q = from_json(r#" {"ver":2, "id":"2401.00001", "title":"t\u00e9", "authors":["A", 7], "comment":null} "#).unwrap();
        assert_eq!((q.id.as_str(), q.ver, q.title.as_str(), q.authors.as_slice()), ("2401.00001", 2, "té", &["A".to_string()][..]));
        assert_eq!(format!("{:?}", from_json(&to_json(&p)).unwrap()), format!("{p:?}"));
        assert!(from_json(&to_json(&p).replace("\"ver\":2", "\"ver\":02")).is_none());
    }

    #[test]
    fn junk_is_rejected() {
        assert!(from_json("").is_none());
        assert!(from_json("{}").is_none());
        assert!(from_json("{\"id\":\"\"}").is_none());
        assert!(from_json("[1,2]").is_none());
    }
}
