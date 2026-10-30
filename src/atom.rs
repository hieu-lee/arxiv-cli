//! Hand-rolled parser for the arXiv Atom API feed (fast, no XML dependency).

use crate::model::Paper;

#[derive(Debug)]
pub struct Feed {
    pub total: usize,
    pub papers: Vec<Paper>,
}

/// Append one text run, collapsing whitespace like `split_whitespace().join(" ")` across run boundaries
/// (`gap` = whitespace seen since the last word).
fn push_text(out: &mut String, gap: &mut bool, run: &str) {
    let is_ws = |c: u8| matches!(c, b' ' | b'\t'..=b'\r');
    if run.is_ascii() {
        let b = run.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if is_ws(b[i]) {
                *gap = true;
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && !is_ws(b[i]) {
                i += 1;
            }
            push_word(out, gap, &run[start..i]);
        }
        return;
    }
    if run.starts_with(char::is_whitespace) {
        *gap = true;
    }
    let mut first = true;
    for w in run.split_whitespace() {
        if !first {
            *gap = true;
        }
        first = false;
        push_word(out, gap, w);
    }
    if run.ends_with(char::is_whitespace) {
        *gap = true;
    }
}

fn push_word(out: &mut String, gap: &mut bool, w: &str) {
    if *gap && !out.is_empty() {
        out.push(' ');
    }
    *gap = false;
    out.push_str(w);
}

/// XML entity decoding and whitespace squashing in one pass (decoded `&#10;` / `&#160;` count as whitespace).
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    let mut rest = s;
    loop {
        let Some(i) = rest.find('&') else {
            push_text(&mut out, &mut gap, rest);
            return out;
        };
        push_text(&mut out, &mut gap, &rest[..i]);
        rest = &rest[i..];
        let semi = rest.as_bytes().iter().take(11).position(|&b| b == b';');
        let rep = semi.and_then(|j| {
            let ent = &rest[1..j];
            match ent {
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
            }
        });
        let (c, used) = match (rep, semi) {
            (Some(c), Some(j)) => (c, j + 1),
            _ => ('&', 1),
        };
        if c.is_whitespace() {
            gap = true;
        } else {
            push_word(&mut out, &mut gap, c.encode_utf8(&mut [0; 4]));
        }
        rest = &rest[used..];
    }
}

/// Offset of the first `<` at or after `from` (a plain byte loop: the gaps between tags are short, so this
/// beats `memchr` setup per call).
#[inline]
fn next_lt(s: &str, from: usize) -> Option<usize> {
    s.as_bytes()[from..].iter().position(|&b| b == b'<').map(|i| from + i)
}

/// Offset of the first `needle` (which starts with `<`) in `s`; hopping between `<` beats a fresh
/// substring-searcher setup on the short blocks parsed here.
fn find_lt(s: &str, needle: &str) -> Option<usize> {
    let mut at = 0;
    while let Some(p) = next_lt(s, at) {
        if s[p..].starts_with(needle) {
            return Some(p);
        }
        at = p + 1;
    }
    None
}

/// End offset of the first `</author>` in `s`, plus whether an `<arxiv:affiliation` tag was seen before it
/// (a false positive only costs the affiliation search).
fn author_end(s: &str) -> Option<(usize, bool)> {
    let (mut at, mut aff) = (0, false);
    while let Some(p) = next_lt(s, at) {
        let t = &s[p..];
        if t.starts_with("</author>") {
            return Some((p, aff));
        }
        aff |= t.starts_with("<arxiv:affiliation");
        at = p + 1;
    }
    None
}

/// Offset of the first `</tag>` in `s[from..]`.
fn find_close(s: &str, from: usize, tag: &str) -> Option<usize> {
    let mut at = from;
    while let Some(i) = find_lt(&s[at..], "</") {
        let p = at + i;
        let t = p + 2;
        if s[t..].starts_with(tag) && s.as_bytes().get(t + tag.len()) == Some(&b'>') {
            return Some(p);
        }
        at = t;
    }
    None
}

/// Text of the element whose opening tag name ends at byte `at` (`at` is the terminator after `<tag`).
fn text_at<'a>(s: &'a str, at: usize, tag: &str) -> Option<&'a str> {
    let gt = s[at..].find('>')? + at;
    if s.as_bytes()[gt - 1] == b'/' {
        return Some("");
    }
    let end = find_close(s, gt + 1, tag)?;
    Some(&s[gt + 1..end])
}

/// Text content of the first `<tag ...>..</tag>` in `s` (tag must match exactly, not as prefix).
fn tag_text<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(p) = next_lt(s, from) {
        let at = p + 1;
        if !s[at..].starts_with(tag) {
            from = at;
            continue;
        }
        let at = at + tag.len();
        if matches!(s.as_bytes().get(at), Some(b'>') | Some(b' ') | Some(b'\n') | Some(b'/')) {
            return text_at(s, at, tag);
        }
        from = at;
    }
    None
}

/// Where the first valid opening tag of each wanted element ends, found in one walk over an entry's tags
/// (equivalent to one `tag_text` search per element, without rescanning the entry each time).
#[derive(Default)]
struct Spots {
    id: Option<usize>,
    title: Option<usize>,
    summary: Option<usize>,
    published: Option<usize>,
    updated: Option<usize>,
    comment: Option<usize>,
    jref: Option<usize>,
    doi: Option<usize>,
    /// Offset of the first `<arxiv:primary_category`.
    primary: Option<usize>,
}

/// One walk over the tags of `e`: fills `Spots` and collects `<category term="..">` values into `cats`.
fn walk_tags(e: &str, cats: &mut Vec<String>) -> Spots {
    let b = e.as_bytes();
    let mut sp = Spots::default();
    let (mut from, mut cat_from, mut cat_open) = (0, 0, true);
    while let Some(p) = next_lt(e, from) {
        from = p + 1;
        if b.get(from) == Some(&b'/') {
            continue;
        }
        let q = from + b[from..].iter().position(|c| matches!(c, b'>' | b' ' | b'\n' | b'\t' | b'\r' | b'/' | b'<')).unwrap_or(b.len() - from);
        let name = &e[from..q];
        let term = b.get(q).copied();
        let slot = match name {
            "id" => &mut sp.id,
            "title" => &mut sp.title,
            "summary" => &mut sp.summary,
            "published" => &mut sp.published,
            "updated" => &mut sp.updated,
            "arxiv:comment" => &mut sp.comment,
            "arxiv:journal_ref" => &mut sp.jref,
            "arxiv:doi" => &mut sp.doi,
            "category" if cat_open && p >= cat_from && term == Some(b' ') => {
                match e[p..].find('>') {
                    None => cat_open = false,
                    Some(g) => {
                        if let Some(t) = attr(&e[p..p + g], "term=\"") {
                            if !cats.iter().any(|c| c == t) {
                                cats.push(t.to_string());
                            }
                        }
                        cat_from = p + g;
                    }
                }
                continue;
            }
            n => {
                if sp.primary.is_none() && n.starts_with("arxiv:primary_category") {
                    sp.primary = Some(p);
                }
                continue;
            }
        };
        if slot.is_none() && matches!(term, Some(b'>' | b' ' | b'\n' | b'/')) {
            *slot = Some(q);
        }
    }
    sp
}

/// Value of `key` (given with its `="` suffix, e.g. `term="`) in a tag's text.
fn attr<'a>(tagtext: &'a str, key: &str) -> Option<&'a str> {
    let i = tagtext.find(key)? + key.len();
    let j = tagtext[i..].find('"')? + i;
    Some(&tagtext[i..j])
}

/// Offset of the next `<name>` / `<name attr..>` opening tag (exact name, not a prefix of another tag).
fn find_open(s: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = next_lt(s, from) {
        from = at + 1;
        if s[from..].starts_with(name) && matches!(s.as_bytes().get(from + name.len()), Some(b'>' | b' ' | b'\n' | b'\t')) {
            return Some(at);
        }
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
        let Some(j) = find_lt(&rest[body_start..], "</entry>") else { break };
        let e = &rest[body_start..body_start + j];
        rest = &rest[body_start + j + 8..];
        let mut cats = Vec::new();
        let sp = walk_tags(e, &mut cats);
        let text = |at: Option<usize>, tag: &str| at.and_then(|at| text_at(e, at, tag));
        let id_url = text(sp.id, "id").unwrap_or("");
        if id_url.contains("/api/errors") {
            let msg = text(sp.summary, "summary").map(clean).unwrap_or_default();
            return Err(format!("arXiv API error: {msg}"));
        }
        let (id, ver) = split_id(id_url.trim());
        let mut p = Paper { id, ver, ..Default::default() };
        p.title = clean(text(sp.title, "title").unwrap_or(""));
        p.abs = clean(text(sp.summary, "summary").unwrap_or(""));
        p.published = text(sp.published, "published").unwrap_or("").trim().to_string();
        p.updated = text(sp.updated, "updated").unwrap_or("").trim().to_string();
        p.comment = text(sp.comment, "arxiv:comment").map(clean).filter(|s| !s.is_empty());
        p.jref = text(sp.jref, "arxiv:journal_ref").map(clean).filter(|s| !s.is_empty());
        p.doi = text(sp.doi, "arxiv:doi").map(clean).filter(|s| !s.is_empty());
        let mut ar = e;
        while let Some(a) = find_open(ar, "author") {
            let Some((b, aff)) = author_end(&ar[a..]).map(|(b, aff)| (a + b, aff)) else { break };
            let Some(gt) = ar[a..].find('>').filter(|&g| a + g < b) else { break };
            let blk = &ar[a + gt + 1..b];
            ar = &ar[b + 9..];
            if let Some(n) = tag_text(blk, "name") {
                p.authors.push(clean(n));
            }
            let mut af = blk;
            while aff {
                let Some(k) = find_lt(af, "<arxiv:affiliation") else { break };
                let Some(t) = tag_text(&af[k..], "arxiv:affiliation") else { break };
                let s = clean(t);
                if !s.is_empty() && !p.affs.contains(&s) {
                    p.affs.push(s);
                }
                af = &af[k + 18..];
            }
        }
        // the primary category comes first; its `term` is the next one at or after the tag
        if let Some(t) = sp.primary.and_then(|k| attr(&e[k..], "term=\"")) {
            cats.retain(|c| c != t);
            cats.insert(0, t.to_string());
        }
        p.cats = cats;
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
    fn repeated_lt_in_cdata_is_linear() {
        let xml = format!("<feed><entry><id>2301.00001</id><summary><![CDATA[{}]]></summary></entry></feed>", "<".repeat(400_000));
        let t = std::time::Instant::now();
        let _ = parse(&xml);
        assert!(t.elapsed().as_secs() < 2, "parse took {:?}", t.elapsed());
    }

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
    fn decoded_whitespace_entities_squash_like_literal_whitespace() {
        let x = "<feed><entry><id>http://arxiv.org/abs/2301.00001v1</id><title> A&#10;B&#160;C  &amp;&lt;D&#x41;&bogus; E&#x110000; </title></entry></feed>";
        assert_eq!(parse(x).unwrap().papers[0].title, "A B C &<DA&bogus; E&#x110000;");
    }
    #[test]
    fn empty_feed() {
        let f = parse("<feed><opensearch:totalResults>0</opensearch:totalResults></feed>").unwrap();
        assert!(f.papers.is_empty());
    }
}
