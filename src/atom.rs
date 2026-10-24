//! Hand-rolled parser for the arXiv Atom API feed (fast, no XML dependency).

use crate::model::Paper;

#[derive(Debug)]
pub struct Feed {
    pub total: usize,
    pub papers: Vec<Paper>,
}

fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match rest.find(';').filter(|&j| j <= 10) {
            Some(j) => {
                let ent = &rest[1..j];
                let rep = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => ent
                        .strip_prefix("#x")
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                        .and_then(char::from_u32),
                };
                match rep {
                    Some(c) => {
                        out.push(c);
                        rest = &rest[j + 1..];
                    }
                    None => {
                        out.push('&');
                        rest = &rest[1..];
                    }
                }
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Text content of the first `<tag ...>..</tag>` in `s` (tag must match exactly, not as prefix).
fn tag_text<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let mut from = 0;
    let open = format!("<{tag}");
    while let Some(i) = s[from..].find(&open) {
        let at = from + i + open.len();
        let next = s.as_bytes().get(at).copied();
        if matches!(next, Some(b'>') | Some(b' ') | Some(b'\n') | Some(b'/')) {
            let gt = s[at..].find('>')? + at;
            if s.as_bytes()[gt - 1] == b'/' {
                return Some("");
            }
            let close = format!("</{tag}>");
            let end = s[gt + 1..].find(&close)? + gt + 1;
            return Some(&s[gt + 1..end]);
        }
        from = at;
    }
    None
}

fn attr<'a>(tagtext: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let i = tagtext.find(&key)? + key.len();
    let j = tagtext[i..].find('"')? + i;
    Some(&tagtext[i..j])
}

/// Offset of the next `<name>` / `<name attr..>` opening tag (exact name, not a prefix of another tag).
fn find_open(s: &str, name: &str) -> Option<usize> {
    let pat = format!("<{name}");
    let mut from = 0;
    while let Some(i) = s[from..].find(&pat) {
        let at = from + i;
        if matches!(s.as_bytes().get(at + pat.len()), Some(b'>' | b' ' | b'\n' | b'\t')) {
            return Some(at);
        }
        from = at + pat.len();
    }
    None
}

fn find_entry(s: &str) -> Option<usize> {
    find_open(s, "entry")
}

/// Split "http://arxiv.org/abs/2301.01234v2" / "…/abs/math/0309285v1" into (id, ver).
pub fn split_id(url: &str) -> (String, u32) {
    let tail = url.rsplit_once("/abs/").map_or(url, |(_, t)| t);
    if let Some(p) = tail.rfind('v') {
        if p > 0 && tail[p + 1..].chars().all(|c| c.is_ascii_digit()) && p + 1 < tail.len() {
            return (tail[..p].to_string(), tail[p + 1..].parse().unwrap_or(0));
        }
    }
    (tail.to_string(), 0)
}

pub fn parse(xml: &str) -> Result<Feed, String> {
    let total = tag_text(xml, "opensearch:totalResults").and_then(|t| t.trim().parse().ok()).unwrap_or(0);
    let mut papers = Vec::new();
    let mut rest = xml;
    while let Some(i) = find_entry(rest) {
        let Some(gt) = rest[i..].find('>') else { break };
        let body_start = i + gt + 1;
        let Some(j) = rest[body_start..].find("</entry>") else { break };
        let e = &rest[body_start..body_start + j];
        rest = &rest[body_start + j + 8..];
        let id_url = tag_text(e, "id").unwrap_or("");
        if id_url.contains("/api/errors") {
            let msg = tag_text(e, "summary").map(|s| squash(&decode(s))).unwrap_or_default();
            return Err(format!("arXiv API error: {msg}"));
        }
        let (id, ver) = split_id(id_url.trim());
        let mut p = Paper { id, ver, ..Default::default() };
        p.title = squash(&decode(tag_text(e, "title").unwrap_or("")));
        p.abs = squash(&decode(tag_text(e, "summary").unwrap_or("")));
        p.published = tag_text(e, "published").unwrap_or("").trim().to_string();
        p.updated = tag_text(e, "updated").unwrap_or("").trim().to_string();
        p.comment = tag_text(e, "arxiv:comment").map(|s| squash(&decode(s))).filter(|s| !s.is_empty());
        p.jref = tag_text(e, "arxiv:journal_ref").map(|s| squash(&decode(s))).filter(|s| !s.is_empty());
        p.doi = tag_text(e, "arxiv:doi").map(|s| squash(&decode(s))).filter(|s| !s.is_empty());
        let mut ar = e;
        while let Some(a) = find_open(ar, "author") {
            let Some(b) = ar[a..].find("</author>") else { break };
            let Some(gt) = ar[a..].find('>').filter(|&g| g < b) else { break };
            let blk = &ar[a + gt + 1..a + b];
            ar = &ar[a + b + 9..];
            if let Some(n) = tag_text(blk, "name") {
                p.authors.push(squash(&decode(n)));
            }
            let mut af = blk;
            while let Some(k) = af.find("<arxiv:affiliation") {
                let Some(t) = tag_text(&af[k..], "arxiv:affiliation") else { break };
                let s = squash(&decode(t));
                if !s.is_empty() && !p.affs.contains(&s) {
                    p.affs.push(s);
                }
                af = &af[k + 18..];
            }
        }
        if let Some(k) = e.find("<arxiv:primary_category") {
            if let Some(t) = attr(&e[k..], "term") {
                p.cats.push(t.to_string());
            }
        }
        let mut cr = e;
        while let Some(k) = cr.find("<category ") {
            let Some(g) = cr[k..].find('>') else { break };
            if let Some(t) = attr(&cr[k..k + g], "term") {
                if !p.cats.iter().any(|c| c == t) {
                    p.cats.push(t.to_string());
                }
            }
            cr = &cr[k + g..];
        }
        if !p.id.is_empty() {
            papers.push(p);
        }
    }
    Ok(Feed { total, papers })
}

#[cfg(test)]
mod tests {
    use super::*;
    const XML: &str = r#"<?xml version='1.0'?><feed xmlns:opensearch="x"><opensearch:totalResults>42</opensearch:totalResults>
<entry><id>http://arxiv.org/abs/math/0309285v2</id><updated>2020-01-02T00:00:00Z</updated><published>2003-09-01T10:00:00Z</published>
<title>On  A &amp; B
 problem</title><summary> Hello &lt;x&gt; &#8211; world </summary>
<author><name>Paul Erd&#337;s</name><arxiv:affiliation xmlns:arxiv="a">Hungarian Acad. Sci.</arxiv:affiliation></author><author><name>T. Tao</name></author>
<arxiv:comment xmlns:arxiv="a">12 pages, accepted</arxiv:comment><arxiv:journal_ref xmlns:arxiv="a">Ann. Math. 1 (2000)</arxiv:journal_ref>
<arxiv:primary_category xmlns:arxiv="a" term="math.CO" scheme="s"/><category term="math.NT" scheme="s"/><category term="math.CO" scheme="s"/></entry></feed>"#;
    #[test]
    fn parses_entry() {
        let f = parse(XML).unwrap();
        assert_eq!(f.total, 42);
        let p = &f.papers[0];
        assert_eq!((p.id.as_str(), p.ver), ("math/0309285", 2));
        assert_eq!(p.title, "On A & B problem");
        assert_eq!(p.abs, "Hello <x> – world");
        assert_eq!(p.authors, ["Paul Erdős", "T. Tao"]);
        assert_eq!(p.affs, ["Hungarian Acad. Sci."]);
        assert_eq!(p.cats, ["math.CO", "math.NT"]);
        assert_eq!(p.comment.as_deref(), Some("12 pages, accepted"));
        assert_eq!(p.jref.as_deref(), Some("Ann. Math. 1 (2000)"));
        assert_eq!(p.doi, None);
        assert_eq!(p.date(), "2003-09-01");
    }
    #[test]
    fn error_entry() {
        let x = "<feed><entry><id>http://arxiv.org/api/errors#x</id><title>Error</title><summary>bad query</summary></entry></feed>";
        assert!(parse(x).unwrap_err().contains("bad query"));
    }
    #[test]
    fn id_split() {
        assert_eq!(split_id("http://arxiv.org/abs/2301.01234v12"), ("2301.01234".into(), 12));
        assert_eq!(split_id("2301.01234"), ("2301.01234".into(), 0));
        assert_eq!(split_id("hep-th/9901001v1"), ("hep-th/9901001".into(), 1));
    }
    #[test]
    fn entry_with_attributes_and_error() {
        let x = r#"<feed><entry xmlns="http://www.w3.org/2005/Atom"><id>http://arxiv.org/api/errors#bad</id><summary>boom</summary></entry></feed>"#;
        assert!(parse(x).unwrap_err().contains("boom"));
        let y = r#"<feed><entry xmlns="a"><id>http://arxiv.org/abs/2301.00001v1</id><title>T</title></entry><entryx>no</entryx></feed>"#;
        assert_eq!(parse(y).unwrap().papers.len(), 1);
    }
    #[test]
    fn author_with_attributes() {
        let y = r#"<feed><entry><id>http://arxiv.org/abs/2301.00001v1</id><title>T</title><author xmlns="http://www.w3.org/2005/Atom"><name>Terence Tao</name></author><authorship>x</authorship></entry></feed>"#;
        assert_eq!(parse(y).unwrap().papers[0].authors, ["Terence Tao"]);
    }
    #[test]
    fn empty_feed() {
        let f = parse("<feed><opensearch:totalResults>0</opensearch:totalResults></feed>").unwrap();
        assert!(f.papers.is_empty());
    }
}
