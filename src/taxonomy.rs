use std::sync::OnceLock;

pub struct Cat {
    pub id: &'static str,
    pub name: &'static str,
}

static RAW: &str = include_str!("../data/cats.tsv");

pub fn all() -> &'static [Cat] {
    static CATS: OnceLock<Vec<Cat>> = OnceLock::new();
    CATS.get_or_init(|| {
        let mut v = Vec::with_capacity(RAW.bytes().filter(|&b| b == b'\n').count() + 1);
        v.extend(RAW.lines().filter_map(|l| {
            let mut p = l.split('\t');
            Some(Cat { id: p.next()?, name: p.next()? })
        }));
        v
    })
}

/// Archives that have `.sub` categories (usable as `arch.*` globs).
fn archive_of(lower: &str) -> Option<&'static str> {
    all().iter().find_map(|c| {
        let (a, _) = c.id.split_once('.')?;
        a.eq_ignore_ascii_case(lower).then_some(a)
    })
}

/// Phrase -> API terms. Phrases are matched token-wise (see `toks`).
static ALIASES: &[(&str, &[&str])] = &[
    ("mathematics", &["math.*"]),
    ("maths", &["math.*"]),
    ("computer science", &["cs.*"]),
    ("statistics", &["stat.*", "math.ST"]),
    ("stats", &["stat.*", "math.ST"]),
    ("economics", &["econ.*"]),
    ("biology", &["q-bio.*"]),
    ("finance", &["q-fin.*"]),
    ("astrophysics", &["astro-ph.*"]),
    ("astro", &["astro-ph.*"]),
    ("condensed matter", &["cond-mat.*"]),
    ("nonlinear", &["nlin.*"]),
    ("hep", &["hep-th", "hep-ph", "hep-ex", "hep-lat"]),
    ("high energy physics", &["hep-th", "hep-ph", "hep-ex", "hep-lat"]),
    ("nuclear", &["nucl-th", "nucl-ex"]),
    ("quantum", &["quant-ph"]),
    ("relativity", &["gr-qc"]),
    ("cosmology", &["astro-ph.CO", "gr-qc"]),
    ("combinatorics", &["math.CO"]),
    ("combinatorial", &["math.CO"]),
    ("discrete math", &["math.CO", "cs.DM"]),
    ("discrete mathematics", &["math.CO", "cs.DM"]),
    ("graph theory", &["math.CO", "cs.DM"]),
    ("graphs", &["math.CO", "cs.DM"]),
    ("number theory", &["math.NT"]),
    ("nt", &["math.NT"]),
    ("geometry", &["math.AG", "math.DG", "math.MG", "math.GT", "math.SG"]),
    ("combinatorial geometry", &["math.MG", "math.CO", "cs.CG"]),
    ("discrete geometry", &["math.MG", "cs.CG"]),
    ("euclidean geometry", &["math.MG"]),
    ("topology", &["math.AT", "math.GN", "math.GT"]),
    ("algebra", &["math.AC", "math.RA", "math.GR", "math.RT", "math.QA", "math.CT"]),
    ("linear algebra", &["math.RA", "math.NA"]),
    ("analysis", &["math.CA", "math.FA", "math.AP", "math.CV"]),
    ("real analysis", &["math.CA"]),
    ("pde", &["math.AP"]),
    ("pdes", &["math.AP"]),
    ("analysis of pdes", &["math.AP"]),
    ("differential equations", &["math.AP", "math.CA", "math.DS"]),
    ("ode", &["math.CA", "math.DS"]),
    ("odes", &["math.CA", "math.DS"]),
    ("probability", &["math.PR"]),
    ("probability theory", &["math.PR"]),
    ("logic", &["math.LO", "cs.LO"]),
    ("set theory", &["math.LO"]),
    ("model theory", &["math.LO"]),
    ("optimization", &["math.OC"]),
    ("control", &["math.OC", "eess.SY"]),
    ("mathematical physics", &["math.MP"]),
    ("mathphys", &["math.MP"]),
    ("numerical analysis", &["math.NA", "cs.NA"]),
    ("information theory", &["cs.IT", "math.IT"]),
    ("history", &["math.HO"]),
    ("machine learning", &["cs.LG", "stat.ML"]),
    ("ml", &["cs.LG", "stat.ML"]),
    ("deep learning", &["cs.LG", "cs.NE"]),
    ("nlp", &["cs.CL"]),
    ("natural language processing", &["cs.CL"]),
    ("llm", &["cs.CL", "cs.LG", "cs.AI"]),
    ("llms", &["cs.CL", "cs.LG", "cs.AI"]),
    ("ai", &["cs.AI"]),
    ("artificial intelligence", &["cs.AI"]),
    ("computer vision", &["cs.CV"]),
    ("cv", &["cs.CV"]),
    ("cryptography", &["cs.CR"]),
    ("security", &["cs.CR"]),
    ("complexity", &["cs.CC"]),
    ("algorithms", &["cs.DS"]),
    ("game theory", &["cs.GT", "econ.TH"]),
    ("theorem proving", &["cs.LO", "cs.AI", "math.LO"]),
    ("formal verification", &["cs.LO", "cs.SE"]),
    ("reinforcement learning", &["cs.LG", "stat.ML"]),
];

const STOP: &[&str] = &["and", "of", "the", "in", "for", "to", "a", "on"];

/// Calls `f` with each lowercase alphanumeric token of `s` (stop words skipped) until it returns false.
fn each_tok(s: &str, cur: &mut Vec<char>, mut f: impl FnMut(&[char]) -> bool) -> bool {
    cur.clear();
    for ch in s.chars().chain(std::iter::once(' ')) {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            if !STOP.iter().any(|w| w.chars().eq(cur.iter().copied())) && !f(cur) {
                return false;
            }
            cur.clear();
        }
    }
    true
}

fn toks(s: &str) -> Vec<Vec<char>> {
    let mut out = Vec::new();
    each_tok(s, &mut Vec::new(), |t| {
        out.push(t.to_vec());
        true
    });
    out
}

/// Equal, or (both >= 5 chars and) edit distance <= 1.
fn tok_eq(a: &[char], b: &[char]) -> bool {
    if a == b {
        return true;
    }
    if a.len() < 5 || b.len() < 5 || a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (s, l) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut i = 0;
    while i < s.len() && s[i] == l[i] {
        i += 1;
    }
    if s.len() == l.len() {
        s[i + 1..] == l[i + 1..]
    } else {
        s[i..] == l[i + 1..]
    }
}

/// `q` equals the tokens of `phrase` (token-wise exact, or within one edit when `fuzzy`).
fn phrase_eq(q: &[Vec<char>], phrase: &str, fuzzy: bool, cur: &mut Vec<char>) -> bool {
    let mut i = 0;
    let all = each_tok(phrase, cur, |t| {
        let hit = q.get(i).is_some_and(|a| if fuzzy { tok_eq(a, t) } else { a == t });
        i += 1;
        hit
    });
    all && i == q.len()
}

fn push_all(out: &mut Vec<String>, ids: impl IntoIterator<Item = impl Into<String>>) {
    for id in ids {
        let id = id.into();
        if !out.contains(&id) {
            out.push(id);
        }
    }
}

/// Resolve user input to arXiv API category terms. Accepts exact ids (case-insens: "math.co"),
/// archive globs ("math", "math.*", "cs.*"), and fuzzy names ("combinatorics" -> math.CO,
/// "number theory" -> math.NT). Returns API-ready terms (ids or globs like "math.*"); empty if unknown.
pub fn resolve(s: &str) -> Vec<String> {
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let lower = s.to_lowercase();
    if let Some(c) = all().iter().find(|c| c.id.eq_ignore_ascii_case(&lower)) {
        return vec![c.id.to_string()];
    }
    if lower.contains('*') {
        let p = lower.trim_end_matches('*').trim_end_matches('.');
        if let Some(a) = archive_of(p) {
            return vec![format!("{a}.*")];
        }
        let mut out = Vec::new();
        if !p.is_empty() && !p.contains('*') {
            push_all(&mut out, all().iter().filter(|c| c.id.to_lowercase().starts_with(p)).map(|c| c.id));
        }
        return out;
    }
    if let Some(a) = archive_of(&lower) {
        return vec![format!("{a}.*")];
    }

    let q = toks(s);
    if q.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for fuzzy in [false, true] {
        for (phrase, ids) in ALIASES {
            if phrase_eq(&q, phrase, fuzzy, &mut cur) {
                push_all(&mut out, ids.iter().copied());
            }
        }
        for c in all() {
            if phrase_eq(&q, c.name, fuzzy, &mut cur) {
                push_all(&mut out, [c.id]);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    for c in all() {
        let n = toks(c.name);
        if q.iter().all(|t| n.iter().any(|w| tok_eq(t, w))) {
            push_all(&mut out, [c.id]);
        }
    }
    if out.len() > 8 {
        out.clear();
    }
    out
}

/// `hay.to_lowercase().contains(needle)` (`needle` already lowercase) without allocating for ASCII text.
pub fn contains_lower(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        true
    } else if hay.is_ascii() {
        hay.as_bytes().windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    } else {
        hay.to_lowercase().contains(needle)
    }
}

/// Compact listing for `arx cats [filter]`.
///
/// Format: `arch: SUB Name | SUB Name ...` (id = `arch.SUB`), one archive per line; legacy
/// single-id archives are `id Name` lines.
pub fn list(filter: Option<&str>) -> String {
    let f = filter.map(|f| f.trim().to_lowercase()).filter(|f| !f.is_empty());
    let mut out = String::with_capacity(if f.is_some() { 256 } else { 4608 });
    let mut open: Option<&str> = None;
    for c in all() {
        if let Some(f) = &f {
            if !contains_lower(c.id, f) && !contains_lower(c.name, f) {
                continue;
            }
        }
        if !out.is_empty() && open.is_none_or(|a| c.id.split_once('.').map(|x| x.0) != Some(a)) {
            out.push('\n');
        }
        match c.id.split_once('.') {
            Some((a, sub)) => {
                if open == Some(a) {
                    out.push_str(" | ");
                } else {
                    out.push_str(a);
                    out.push_str(": ");
                    open = Some(a);
                }
                out.push_str(sub);
                out.push(' ');
                out.push_str(c.name);
            }
            None => {
                open = None;
                out.push_str(c.id);
                out.push(' ');
                out.push_str(c.name);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Vec<String> {
        resolve(s)
    }

    #[test]
    fn data_complete() {
        assert_eq!(all().len(), 155);
        for c in all() {
            assert!(!c.id.is_empty() && !c.name.is_empty(), "{}", c.id);
        }
        let mut ids: Vec<_> = all().iter().map(|c| c.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), all().len());
        for id in ["math.AG", "math.ST", "math-ph", "cs.LG", "stat.ML", "eess.SP", "econ.TH", "q-bio.QM", "q-fin.MF", "physics.optics", "astro-ph.CO", "cond-mat.soft", "gr-qc", "hep-th", "nlin.CD", "nucl-ex", "quant-ph"] {
            assert!(all().iter().any(|c| c.id == id), "{id}");
        }
    }

    #[test]
    fn exact_and_glob() {
        assert_eq!(r("math.co"), ["math.CO"]);
        assert_eq!(r(" MATH.NT "), ["math.NT"]);
        assert_eq!(r("quant-ph"), ["quant-ph"]);
        assert_eq!(r("Math-PH"), ["math-ph"]);
        assert_eq!(r("gr-qc"), ["gr-qc"]);
        assert_eq!(r("math"), ["math.*"]);
        assert_eq!(r("math.*"), ["math.*"]);
        assert_eq!(r("CS.*"), ["cs.*"]);
        assert_eq!(r("q-bio"), ["q-bio.*"]);
        assert_eq!(r("astro-ph.*"), ["astro-ph.*"]);
        assert_eq!(r("math.C*"), ["math.CA", "math.CO", "math.CT", "math.CV"]);
        assert_eq!(r("zzz.*"), Vec::<String>::new());
    }

    #[test]
    fn fuzzy_names() {
        assert_eq!(r("combinatorics"), ["math.CO"]);
        assert_eq!(r("Number Theory"), ["math.NT"]);
        assert_eq!(r("number-theory"), ["math.NT"]);
        assert_eq!(r("geometry"), ["math.AG", "math.DG", "math.MG", "math.GT", "math.SG"]);
        assert_eq!(r("probability"), ["math.PR"]);
        assert_eq!(r("analysis of pdes"), ["math.AP"]);
        assert_eq!(r("logic"), ["math.LO", "cs.LO"]);
        assert_eq!(r("machine learning"), ["cs.LG", "stat.ML"]);
        assert_eq!(r("nlp"), ["cs.CL"]);
        assert_eq!(r("ai"), ["cs.AI"]);
        assert_eq!(r("algebraic geometry"), ["math.AG"]);
        assert_eq!(r("functional analysis"), ["math.FA"]);
        assert_eq!(r("group theory"), ["math.GR"]);
        assert_eq!(r("computational geometry"), ["cs.CG"]);
        assert_eq!(r("computer science"), ["cs.*"]);
    }

    #[test]
    fn fuzzy_typos_and_subsets() {
        assert_eq!(r("combinatorcs"), ["math.CO"]);
        assert_eq!(r("probabilty"), ["math.PR"]);
        assert_eq!(r("algebraic geometri"), ["math.AG"]);
        assert_eq!(r("symplectic"), ["math.SG"]);
        assert_eq!(r("operator"), ["math.OA"]);
        assert_eq!(r("fluid"), ["physics.flu-dyn"]);
    }

    #[test]
    fn unknown_and_junk() {
        for s in ["", "   ", "*", ".", "xyzzy", "theory", "日本語", "é.*", "a\tb", "and of the"] {
            assert!(r(s).is_empty(), "{s:?} -> {:?}", r(s));
        }
        assert!(r("日本語 combinatorics").is_empty());
    }

    #[test]
    fn contains_lower_matches_str_contains() {
        for (h, n) in [("Math.CO", ""), ("", ""), ("Math.CO", "co"), ("Math.CO", "x"), ("Ünï Math", "math"), ("ab", "abc")] {
            assert_eq!(contains_lower(h, n), h.to_lowercase().contains(n), "{h:?} {n:?}");
        }
    }

    #[test]
    fn list_format() {
        let l = list(None);
        assert_eq!(l.lines().count(), 11 + 9);
        assert!(l.lines().any(|x| x.starts_with("math: AC Commutative Algebra | AG Algebraic Geometry | ")));
        assert!(l.lines().any(|x| x == "quant-ph Quantum Physics"));
        assert!(l.lines().any(|x| x.starts_with("physics: acc-ph Accelerator Physics | ")));
        assert_eq!(list(Some("combinatorics")), "math: CO Combinatorics");
        assert_eq!(list(Some("math.nt")), "math: NT Number Theory");
        assert_eq!(list(Some("quantum phys")), "quant-ph Quantum Physics");
        assert_eq!(list(Some("number theory")), "math: NT Number Theory");
        assert_eq!(list(Some("zzzz")), "");
        assert_eq!(list(Some("gr-qc")), "gr-qc General Relativity and Quantum Cosmology");
        assert_eq!(list(Some("")), l);
    }
}
