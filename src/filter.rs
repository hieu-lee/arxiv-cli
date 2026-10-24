//! Client-side post filters (author, institution, publication status, venue quartile, comment, ...).

use crate::args::Args;
use crate::fuzzy::{contains_fuzzy, contains_fuzzy_strict, AuthorQ};
use crate::model::Paper;

const DEFAULT_FUZZ: f32 = 0.82;
const JN_MIN: f32 = 0.85;
const ACCEPT_CUES: &[&str] = &[
    "accepted", "to appear", "in press", "camera-ready", "camera ready", "published in", "appears in",
    "will appear", "forthcoming", "appeared in", "appeared as", "published as", "published at", "published version",
];

type Range = (Option<u32>, Option<u32>);
/// Outer = AND (repeated flag), inner = OR (comma list).
type Groups = Vec<Vec<String>>;

#[derive(Default)]
pub struct Filter {
    au: Vec<Vec<AuthorQ>>,
    inst: Groups,
    jn: Groups,
    cmt: Groups,
    msc: Groups,
    pcat: Groups,
    published: bool,
    preprint: bool,
    accepted: bool,
    doi: bool,
    /// Bit (q-1) set for each accepted quartile.
    q: Option<u8>,
    pages: Option<Range>,
    nau: Option<Range>,
    fuzz: f32,
}

/// Parse "N", "N-", "-N", "N-M" into inclusive (min, max) bounds.
pub fn parse_range(s: &str) -> Result<(Option<u32>, Option<u32>), String> {
    let t = s.trim();
    let num = |x: &str| x.trim().parse::<u32>().map_err(|_| format!("bad range {s:?} (use N, N-, -N or N-M)"));
    match t.split_once('-') {
        None => num(t).map(|n| (Some(n), Some(n))),
        Some((a, b)) => {
            let lo = if a.trim().is_empty() { None } else { Some(num(a)?) };
            let hi = if b.trim().is_empty() { None } else { Some(num(b)?) };
            match (lo, hi) {
                (None, None) => Err(format!("bad range {s:?} (use N, N-, -N or N-M)")),
                (Some(l), Some(h)) if l > h => Err(format!("bad range {s:?}: min {l} > max {h}")),
                _ => Ok((lo, hi)),
            }
        }
    }
}

/// Parse "1", "1,2", "1-2", "q1", "Q1,q3-4" into a bitmask where bit (q-1) is quartile q.
pub fn parse_quartiles(s: &str) -> Result<u8, String> {
    let bad = || format!("bad quartile {s:?} (use 1 | 1,2 | 1-2 | q1)");
    let q = |x: &str| -> Result<u8, String> {
        let x = x.trim();
        let x = x.strip_prefix(['q', 'Q']).unwrap_or(x);
        match x.parse::<u8>() {
            Ok(n @ 1..=4) => Ok(n),
            _ => Err(bad()),
        }
    };
    let mut mask = 0u8;
    for part in s.split(',') {
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (q(a)?, q(b)?),
            None => {
                let n = q(part)?;
                (n, n)
            }
        };
        if lo > hi {
            return Err(bad());
        }
        for n in lo..=hi {
            mask |= 1 << (n - 1);
        }
    }
    Ok(mask)
}

fn lower_groups(a: &Args, name: &str) -> Groups {
    a.groups(name).into_iter().map(|g| g.into_iter().map(|s| s.to_lowercase()).collect()).collect()
}

fn contains_ci(hay: &str, needle_lower: &str) -> bool {
    if needle_lower.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle_lower.as_bytes());
        n.is_empty() || h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        hay.to_lowercase().contains(needle_lower)
    }
}

/// `lower[a..b]` is not glued to neighbouring letters ("in press" must not match "in pressurized").
fn word_bounded(lower: &str, a: usize, b: usize) -> bool {
    let before = lower[..a].chars().next_back().map_or(true, |c| !c.is_alphanumeric());
    let after = lower[b..].chars().next().map_or(true, |c| !c.is_alphanumeric());
    before && after
}

fn has_accept_cue(comment: Option<&str>) -> bool {
    let Some(c) = comment else { return false };
    // "not accepted to X but accepted to Y": the contrast starts a new clause
    let lower = c.to_lowercase().replace(" but ", "; ").replace('\u{2019}', "'");
    ACCEPT_CUES.iter().any(|cue| {
        lower.match_indices(cue).filter(|(i, m)| word_bounded(&lower, *i, i + m.len())).any(|(i, _)| {
            // negation anywhere earlier in the same clause: "not accepted", "has not been accepted", "not yet accepted"
            let clause = lower[..i].rsplit(['.', ';', ',']).next().unwrap_or("");
            !clause.split(|c: char| !c.is_alphanumeric() && c != '\'').any(|w| matches!(w, "not" | "never" | "yet" | "un" | "isn't" | "hasn't" | "wasn't" | "weren't" | "won't"))
        })
    })
}

/// First "N pages" / "N page" / "N pp" occurrence in an arXiv comment.
fn parse_pages(comment: &str) -> Option<u32> {
    let b = comment.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let n = comment[start..i].parse::<u32>().ok();
        let mut j = i;
        if b.get(j) == Some(&b' ') {
            j += 1;
        }
        let rest = &b[j..];
        let word = |w: &[u8]| rest.len() >= w.len() && rest[..w.len()].eq_ignore_ascii_case(w);
        let boundary = |k: usize| !rest.get(k).is_some_and(u8::is_ascii_alphabetic);
        if n.is_some() && ((word(b"page") && (boundary(4) || (word(b"pages") && boundary(5)))) || (word(b"pp") && boundary(2))) {
            return n;
        }
    }
    None
}

fn in_range(v: u32, r: Range) -> bool {
    r.0.map_or(true, |lo| v >= lo) && r.1.map_or(true, |hi| v <= hi)
}

fn glob_ci(pat: &str, text: &str) -> bool {
    let (p, t) = (pat.as_bytes(), text.as_bytes());
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while j < t.len() {
        if i < p.len() && p[i] == b'*' {
            star = Some((i, j));
            i += 1;
        } else if i < p.len() && p[i].eq_ignore_ascii_case(&t[j]) {
            i += 1;
            j += 1;
        } else if let Some((si, sj)) = star {
            i = si + 1;
            j = sj + 1;
            star = Some((si, sj + 1));
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == b'*')
}

fn pcat_match(pat: &str, primary: &str) -> bool {
    glob_ci(pat, primary)
        || (!pat.contains(['.', '*']) && primary.len() > pat.len() && primary.as_bytes()[pat.len()] == b'.' && glob_ci(pat, &primary[..pat.len()]))
}

impl Filter {
    pub fn from_args(a: &Args) -> Result<Filter, String> {
        let fuzz = match a.get("fuzz") {
            None => DEFAULT_FUZZ,
            Some(v) => match v.trim().parse::<f32>() {
                Ok(x) if (0.0..=1.0).contains(&x) => x,
                _ => return Err(format!("--fuzz must be a number in 0..1, got {v:?}")),
            },
        };
        // repeated range flags are AND: intersect (tightest lower and upper bound)
        let range = |name: &str| -> Result<Option<Range>, String> {
            let mut acc: Option<Range> = None;
            for v in a.all(name) {
                let (lo, hi) = parse_range(v).map_err(|e| format!("--{name}: {e}"))?;
                acc = Some(match acc {
                    None => (lo, hi),
                    Some((l0, h0)) => (l0.max(lo), match (h0, hi) {
                        (Some(x), Some(y)) => Some(x.min(y)),
                        (x, y) => x.or(y),
                    }),
                });
            }
            Ok(acc)
        };
        let f = Filter {
            au: a.groups("au").into_iter().map(|g| g.iter().map(|s| AuthorQ::new(s)).collect()).collect(),
            inst: a.groups("inst"),
            jn: a.groups("jn"),
            cmt: lower_groups(a, "cmt"),
            msc: lower_groups(a, "msc"),
            pcat: lower_groups(a, "pcat"),
            published: a.flag("published"),
            preprint: a.flag("preprint"),
            accepted: a.flag("accepted"),
            doi: a.flag("doi"),
            // repeated --q is AND: only quartiles allowed by every occurrence survive
            q: a.all("q").into_iter().map(parse_quartiles).collect::<Result<Vec<u8>, _>>().map_err(|e| format!("--q: {e}"))?.into_iter().reduce(|x, y| x & y),
            pages: range("pages")?,
            nau: range("nau")?,
            fuzz,
        };
        if f.preprint && (f.published || f.accepted || f.q.is_some()) {
            return Err("--preprint conflicts with --published/--accepted/--q".into());
        }
        Ok(f)
    }

    /// Needs HTML-derived institutions (--inst).
    pub fn needs_insts(&self) -> bool {
        !self.inst.is_empty()
    }
    /// Needs MSC classes from the abs page (--msc).
    pub fn needs_msc(&self) -> bool {
        !self.msc.is_empty()
    }
    /// Error when `p` comes from a substitute source (OpenAlex) that lacks data an active filter depends on,
    /// instead of silently treating "unknown" as "does not match".
    pub fn check_provenance(&self, p: &Paper) -> Result<(), String> {
        if p.partial & 1 != 0 && (!self.cmt.is_empty() || self.pages.is_some() || !self.pcat.is_empty() || self.preprint || self.accepted) {
            return Err("--cmt/--pages/--pcat/--preprint/--accepted need arXiv metadata (categories, author comments) that the substitute source (OpenAlex) lacks; retry when arXiv is reachable (--via api) or use cached data (--via cache)".into());
        }
        if p.partial & 2 != 0 && (self.nau.is_some() || !self.au.is_empty()) {
            return Err("--nau/--au: the substitute source (OpenAlex) truncates author lists at 100".into());
        }
        Ok(())
    }

    /// Any client-side filter active at all (decides whether we must over-fetch).
    pub fn any(&self) -> bool {
        !(self.au.is_empty() && self.inst.is_empty() && self.jn.is_empty() && self.cmt.is_empty() && self.msc.is_empty() && self.pcat.is_empty())
            || self.published
            || self.preprint
            || self.accepted
            || self.doi
            || self.q.is_some()
            || self.pages.is_some()
            || self.nau.is_some()
    }

    fn ensure_venue(p: &mut Paper) {
        if p.venue.is_none() {
            p.venue = p.jref.as_deref().and_then(crate::venue::resolve);
        }
        if p.venue.is_none() {
            p.venue = p.comment.as_deref().and_then(crate::venue::resolve_comment);
        }
    }

    /// Cheap checks on API metadata only: author, published/preprint/accepted, quartile, jn, cmt, pages, nau, pcat, doi.
    /// Also sets `p.venue`. Must not look at insts/msc.
    pub fn pass_meta(&self, p: &mut Paper) -> bool {
        if self.doi && p.doi.is_none() {
            return false;
        }
        if self.published && !p.is_published() {
            return false;
        }
        if let Some(r) = self.nau {
            if !in_range(p.authors.len() as u32, r) {
                return false;
            }
        }
        if !self.pcat.is_empty() {
            let primary = p.primary();
            if !self.pcat.iter().all(|g| g.iter().any(|pat| pcat_match(pat, primary))) {
                return false;
            }
        }
        if let Some(r) = self.pages {
            match p.comment.as_deref().and_then(parse_pages) {
                Some(n) if in_range(n, r) => {}
                _ => return false,
            }
        }
        if !self.cmt.is_empty() {
            let Some(c) = p.comment.as_deref() else { return false };
            if !self.cmt.iter().all(|g| g.iter().any(|n| contains_ci(c, n))) {
                return false;
            }
        }
        if self.preprint || self.accepted {
            let accepted = p.is_published() || has_accept_cue(p.comment.as_deref());
            if accepted != self.accepted {
                return false;
            }
        }
        if let Some(mask) = self.q {
            Self::ensure_venue(p);
            if !p.venue.is_some_and(|v| (1..=4).contains(&v.q) && mask >> (v.q - 1) & 1 == 1) {
                return false;
            }
        }
        if !self.jn.is_empty() {
            let Some(j) = p.jref.as_deref() else { return false };
            let thr = self.fuzz.max(JN_MIN);
            // when both the reference and the query name a *known* journal, canonical equality decides
            // ("Commun. Math. Phys." = full name; "Physical Review A" != "Physical Review Applied");
            // otherwise fall back to fuzzy text matching
            let name_matches = |n: &str| {
                let v = crate::venue::resolve(j);
                // canonical equality only when the query names one journal unambiguously (exact, or a clear winner)
                let top = crate::venue::lookup(n, 2);
                let w = top.first().copied().filter(|(_, s)| {
                    *s >= 0.85 && (*s >= 0.999 || top.get(1).map_or(true, |(_, s2)| *s2 < *s - 0.03))
                });
                match (v, w) {
                    (Some(v), Some((w, _))) => v.name == w.name,
                    _ => contains_fuzzy(j, n) >= thr,
                }
            };
            if !self.jn.iter().all(|g| g.iter().any(|n| name_matches(n))) {
                return false;
            }
        }
        if !self.au.is_empty()
            && !self.au.iter().all(|g| g.iter().any(|q| p.authors.iter().any(|a| q.score(a) >= self.fuzz)))
        {
            return false;
        }
        Self::ensure_venue(p);
        true
    }

    /// Checks needing enrichment (insts, msc). Call only after enrichment.
    pub fn pass_enriched(&self, p: &Paper) -> bool {
        if !self.msc.is_empty() {
            let ok = self.msc.iter().all(|g| {
                g.iter().any(|pre| p.msc.iter().any(|m| m.len() >= pre.len() && m.as_bytes()[..pre.len()].eq_ignore_ascii_case(pre.as_bytes())))
            });
            if !ok {
                return false;
            }
        }
        if !self.inst.is_empty() {
            let ok = self.inst.iter().all(|g| {
                g.iter().any(|n| p.insts.iter().chain(&p.affs).any(|i| contains_fuzzy_strict(i, n) >= self.fuzz))
            });
            if !ok {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::parse;
    use crate::model::Venue;

    fn flt(args: &[&str]) -> Filter {
        let toks: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        Filter::from_args(&parse(&toks).unwrap()).unwrap()
    }
    fn err(args: &[&str]) -> String {
        let toks: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        Filter::from_args(&parse(&toks).unwrap()).err().unwrap()
    }
    fn paper() -> Paper {
        Paper {
            id: "2301.00001".into(),
            authors: vec!["Terence Tao".into(), "Ben Green".into()],
            cats: vec!["math.CO".into(), "math.NT".into()],
            venue: Some(venue(4)),
            ..Default::default()
        }
    }
    fn venue(q: u8) -> Venue {
        Venue { name: "Annals of Mathematics", q, area: "math" }
    }
    fn run(f: &Filter, mut p: Paper) -> bool {
        f.pass_meta(&mut p) && f.pass_enriched(&p)
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("20"), Ok((Some(20), Some(20))));
        assert_eq!(parse_range("10-"), Ok((Some(10), None)));
        assert_eq!(parse_range("-30"), Ok((None, Some(30))));
        assert_eq!(parse_range(" 10 - 30 "), Ok((Some(10), Some(30))));
        assert_eq!(parse_range("0-0"), Ok((Some(0), Some(0))));
        for bad in ["", "-", "a", "3-1", "1-2-3", "--5", "1.5"] {
            assert!(parse_range(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn jn_partial_names_stay_fuzzy() {
        let f = |args: &[&str]| Filter::from_args(&crate::args::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()).unwrap();
        let mut p = Paper { jref: Some("Physical Review APPLIED 26, 044035 (2026)".into()), ..Default::default() };
        assert!(f(&["--jn", "Physical Review"]).pass_meta(&mut p));
        assert!(f(&["--jn", "Physical Review Applied"]).pass_meta(&mut p));
    }

    #[test]
    fn jn_uses_canonical_equality_for_known_journals() {
        let f = |args: &[&str]| Filter::from_args(&crate::args::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()).unwrap();
        let mut p = Paper { jref: Some("Physical Review APPLIED 26, 044035 (2026)".into()), ..Default::default() };
        assert!(!f(&["--jn", "Physical Review A"]).pass_meta(&mut p));
        let mut q = Paper { jref: Some("Phys. Rev. A 105, 022411 (2022)".into()), ..Default::default() };
        assert!(f(&["--jn", "Physical Review A"]).pass_meta(&mut q));
    }

    #[test]
    fn substitute_source_papers_cannot_satisfy_arxiv_only_filters() {
        let f = |args: &[&str]| Filter::from_args(&crate::args::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()).unwrap();
        let oa = Paper { partial: 1, ..Default::default() };
        assert!(f(&["--cmt", "x"]).check_provenance(&oa).is_err());
        assert!(f(&["--pages", "10-"]).check_provenance(&oa).is_err());
        assert!(f(&["--pcat", "math.*"]).check_provenance(&oa).is_err());
        assert!(f(&["--accepted"]).check_provenance(&oa).is_err());
        assert!(f(&["--published"]).check_provenance(&oa).is_ok());
        assert!(f(&["--nau", "3"]).check_provenance(&oa).is_ok());
        assert!(f(&["--nau", "3"]).check_provenance(&Paper { partial: 3, ..Default::default() }).is_err());
        assert!(f(&["--au", "tao"]).check_provenance(&Paper { partial: 3, ..Default::default() }).is_err());
        assert!(f(&["--au", "tao"]).check_provenance(&oa).is_ok());
        assert!(f(&["--cmt", "x"]).check_provenance(&Paper::default()).is_ok());
    }

    #[test]
    fn repeated_ranges_intersect() {
        let f = |args: &[&str]| Filter::from_args(&crate::args::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()).unwrap();
        assert_eq!(f(&["--nau", "10-", "--nau", "-10"]).nau, Some((Some(10), Some(10))));
        assert_eq!(f(&["--pages", "5-30", "--pages", "10-"]).pages, Some((Some(10), Some(30))));
    }

    #[test]
    fn repeated_q_is_intersection() {
        let f = |args: &[&str]| Filter::from_args(&crate::args::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()).unwrap();
        assert_eq!(f(&["--q", "1,2", "--q", "2,3"]).q, Some(0b0010));
        assert_eq!(f(&["--q", "2", "--q", "1"]).q, Some(0));
    }

    #[test]
    fn quartiles() {
        assert_eq!(parse_quartiles("1"), Ok(0b0001));
        assert_eq!(parse_quartiles("1,2"), Ok(0b0011));
        assert_eq!(parse_quartiles("1-2"), Ok(0b0011));
        assert_eq!(parse_quartiles("q1"), Ok(0b0001));
        assert_eq!(parse_quartiles("Q1, q3-4"), Ok(0b1101));
        for bad in ["", "0", "5", "x", "2-1", "1,", "1-"] {
            assert!(parse_quartiles(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bad_args_are_one_line_errors() {
        for a in [&["--pages", "5-1"][..], &["--nau", "x"], &["--q", "9"], &["--fuzz", "2"], &["--fuzz", "abc"], &["--preprint", "--published"]] {
            let e = err(a);
            assert!(!e.is_empty() && !e.contains('\n'), "{e}");
        }
        assert!(err(&["--q", "9"]).starts_with("--q:"));
    }

    #[test]
    fn any_and_needs() {
        assert!(!flt(&[]).any());
        assert!(!flt(&["--fuzz", "0.9"]).any());
        for a in [&["--au", "x"][..], &["--inst", "x"], &["--published"], &["--preprint"], &["--accepted"], &["--q", "1"], &["--jn", "x"], &["--cmt", "x"], &["--pages", "3"], &["--nau", "3"], &["--msc", "11"], &["--pcat", "math.*"], &["--doi"]] {
            assert!(flt(a).any(), "{a:?}");
        }
        assert!(flt(&["--inst", "mit"]).needs_insts());
        assert!(!flt(&["--msc", "11"]).needs_insts());
        assert!(flt(&["--msc", "11"]).needs_msc());
        assert!(!flt(&[]).needs_msc());
    }

    #[test]
    fn author_and_or() {
        let f = flt(&["--au", "tao"]);
        assert!(run(&f, paper()));
        assert!(!run(&flt(&["--au", "gowers"]), paper()));
        assert!(run(&flt(&["--au", "gowers,green"]), paper()));
        assert!(run(&flt(&["--au", "tao", "--au", "ben green"]), paper()));
        assert!(!run(&flt(&["--au", "tao", "--au", "gowers"]), paper()));
        assert!(!run(&flt(&["--au", "S. Tao"]), paper()));
        assert!(run(&flt(&["--au", "Terrence Tao"]), paper()));
        assert!(!run(&flt(&["--au", "terrence tao", "--fuzz", "0.99"]), paper()));
        assert!(run(&flt(&["--au", "Erdos"]), Paper { authors: vec!["Paul Erdős".into()], ..paper() }));
    }

    #[test]
    fn status_filters() {
        let jr = Paper { jref: Some("Ann. of Math. 1 (2020)".into()), venue: Some(venue(1)), ..paper() };
        let doi = Paper { doi: Some("10.1/x".into()), ..paper() };
        let cue = Paper { comment: Some("12 pages. To appear in Acta".into()), ..paper() };
        let plain = Paper { comment: Some("12 pages, 3 figures".into()), ..paper() };
        let published = flt(&["--published"]);
        assert!(run(&published, jr.clone()));
        assert!(run(&published, doi.clone()));
        assert!(!run(&published, cue.clone()));
        assert!(!run(&published, plain.clone()));
        let accepted = flt(&["--accepted"]);
        assert!(run(&accepted, jr.clone()));
        assert!(run(&accepted, doi.clone()));
        assert!(run(&accepted, cue.clone()));
        assert!(!run(&accepted, plain.clone()));
        assert!(!run(&accepted, paper()));
        let pre = flt(&["--preprint"]);
        assert!(!run(&pre, jr));
        assert!(!run(&pre, doi));
        assert!(!run(&pre, cue));
        assert!(run(&pre, plain));
        assert!(run(&pre, paper()));
        let has_doi = flt(&["--doi"]);
        assert!(!run(&has_doi, Paper { jref: Some("x".into()), venue: Some(venue(1)), ..paper() }));
        assert!(run(&has_doi, Paper { doi: Some("10.1/x".into()), ..paper() }));
        for c in ["Accepted for publication", "IN PRESS", "camera-ready version", "Camera ready", "Published in JAMS", "appears in Proc."] {
            assert!(run(&accepted, Paper { comment: Some(c.into()), ..paper() }), "{c}");
        }
    }

    #[test]
    fn quartile_filter() {
        let mk = |q: Option<u8>| Paper { jref: Some("J. X 1".into()), venue: q.map(venue), ..paper() };
        let f = flt(&["--q", "1,2"]);
        assert!(run(&f, mk(Some(1))));
        assert!(run(&f, mk(Some(2))));
        assert!(!run(&f, mk(Some(3))));
        assert!(!run(&f, mk(Some(4))));
        assert!(!run(&f, Paper { venue: None, ..paper() }));
        assert!(run(&flt(&["--q", "q4"]), mk(Some(4))));
        assert!(!run(&flt(&["--q", "1-2"]), mk(Some(4))));
    }

    #[test]
    fn meta_text_filters() {
        let with = |c: &str| Paper { comment: Some(c.into()), ..paper() };
        let f = flt(&["--cmt", "figures"]);
        assert!(run(&f, with("5 Figures")));
        assert!(!run(&f, with("5 pages")));
        assert!(!run(&f, paper()));
        assert!(run(&flt(&["--cmt", "lemma,figures"]), with("5 figures")));
        assert!(run(&flt(&["--cmt", "5", "--cmt", "fig"]), with("5 figures")));
        assert!(!run(&flt(&["--cmt", "5", "--cmt", "tables"]), with("5 figures")));
        assert!(run(&flt(&["--cmt", "Zürich"]), with("talk at ZÜRICH")));

        let jn = Paper { jref: Some("Ann. of Math. 198 (2023) 1-50".into()), venue: Some(venue(1)), ..paper() };
        assert!(run(&flt(&["--jn", "Annals of Mathematics"]), jn.clone()));
        assert!(run(&flt(&["--jn", "Inventiones,Annals of Mathematics"]), jn.clone()));
        assert!(!run(&flt(&["--jn", "Inventiones"]), jn));
        assert!(!run(&flt(&["--jn", "Annals"]), paper()));
    }

    #[test]
    fn pages_filter() {
        let with = |c: &str| Paper { comment: Some(c.into()), ..paper() };
        let f = flt(&["--pages", "10-30"]);
        assert!(run(&f, with("12 pages, 3 figures")));
        assert!(run(&f, with("10 pages")));
        assert!(run(&f, with("30 Pages")));
        assert!(run(&f, with("v2: 20 pp.")));
        assert!(!run(&f, with("31 pages")));
        assert!(!run(&f, with("9 pages")));
        assert!(!run(&f, with("no count here")));
        assert!(!run(&f, with("Section 3 pagesetter")));
        assert!(!run(&f, paper()));
        assert!(run(&flt(&["--pages", "25-"]), with("40 pages")));
        assert!(!run(&flt(&["--pages", "25-"]), with("24 pages")));
        assert!(run(&flt(&["--pages", "-5"]), with("5 pages")));
        assert!(run(&flt(&["--pages", "7"]), with("7pages")));
        assert_eq!(parse_pages("5+10 pages, 2 figures"), Some(10));
        assert_eq!(parse_pages("1 page"), Some(1));
        assert_eq!(parse_pages("99999999999 pages"), None);
    }

    #[test]
    fn nau_and_pcat() {
        assert!(run(&flt(&["--nau", "2"]), paper()));
        assert!(!run(&flt(&["--nau", "3-"]), paper()));
        assert!(run(&flt(&["--nau", "-2"]), paper()));
        assert!(run(&flt(&["--pcat", "math.*"]), paper()));
        assert!(run(&flt(&["--pcat", "math"]), paper()));
        assert!(run(&flt(&["--pcat", "MATH.co"]), paper()));
        assert!(run(&flt(&["--pcat", "cs.LO,math.CO"]), paper()));
        assert!(!run(&flt(&["--pcat", "math.NT"]), paper()));
        assert!(!run(&flt(&["--pcat", "cs.*"]), paper()));
        assert!(!run(&flt(&["--pcat", "mat"]), paper()));
        assert!(run(&flt(&["--pcat", "*.CO"]), paper()));
        let old = Paper { cats: vec!["math-ph".into()], ..paper() };
        assert!(run(&flt(&["--pcat", "math-ph"]), old.clone()));
        assert!(!run(&flt(&["--pcat", "math"]), old));
        assert!(!run(&flt(&["--pcat", "math.*"]), Paper { cats: vec![], ..paper() }));
    }

    #[test]
    fn enriched_filters() {
        let p = Paper {
            msc: vec!["11B13".into(), "05C69".into()],
            insts: vec!["Massachusetts Institute of Technology".into(), "University of Cambridge".into()],
            affs: vec!["UCLA".into()],
            ..paper()
        };
        assert!(run(&flt(&["--msc", "11"]), p.clone()));
        assert!(run(&flt(&["--msc", "05c"]), p.clone()));
        assert!(run(&flt(&["--msc", "03,05C69"]), p.clone()));
        assert!(!run(&flt(&["--msc", "03"]), p.clone()));
        assert!(!run(&flt(&["--msc", "11B130"]), p.clone()));
        assert!(!run(&flt(&["--msc", "11", "--msc", "03"]), p.clone()));
        assert!(run(&flt(&["--msc", "11", "--msc", "05"]), p.clone()));
        assert!(run(&flt(&["--inst", "MIT"]), p.clone()));
        assert!(run(&flt(&["--inst", "Stanford,Cambridge"]), p.clone()));
        assert!(run(&flt(&["--inst", "UCLA"]), p.clone()));
        assert!(run(&flt(&["--inst", "mit", "--inst", "univ. of cambridge"]), p.clone()));
        assert!(!run(&flt(&["--inst", "mit", "--inst", "Harvard"]), p.clone()));
        assert!(!run(&flt(&["--inst", "Harvard"]), p.clone()));
        assert!(!run(&flt(&["--inst", "MIT"]), paper()));
        assert!(!run(&flt(&["--msc", "11"]), paper()));
    }

    #[test]
    fn combined_and_empty() {
        assert!(run(&flt(&[]), paper()));
        let p = Paper { comment: Some("12 pages".into()), ..paper() };
        assert!(run(&flt(&["--au", "tao", "--pages", "10-", "--pcat", "math.*", "--nau", "2"]), p.clone()));
        assert!(!run(&flt(&["--au", "tao", "--pages", "13-"]), p));
    }

    #[test]
    fn negated_acceptance_is_not_acceptance() {
        assert!(has_accept_cue(Some("Accepted to FOCS 2024")));
        assert!(!has_accept_cue(Some("Experiments in pressurized fluids; submitted to Physics of Fluids")));
        assert!(has_accept_cue(Some("In press at JMLR")));
        assert!(has_accept_cue(Some("Forthcoming in Annals of Mathematics")));
        assert!(!has_accept_cue(Some("The forest LR rule now appears as v2 of arXiv:2606.23876")));
        assert!(has_accept_cue(Some("48 pages, no figures. This is the published version")));
        assert!(has_accept_cue(Some("Appeared as Annals of Math. 176 (2012), 1231--1372")));
        assert!(has_accept_cue(Some("Published as a main conference paper at NeurIPS 2026")));
        assert!(has_accept_cue(Some("12 pages; to appear in Annals")));
        assert!(!has_accept_cue(Some("Not accepted to ICML 2024")));
        assert!(!has_accept_cue(Some("not yet accepted")));
        assert!(!has_accept_cue(Some("34 pages, short journal version submitted for peer review")));
        assert!(!has_accept_cue(Some("Submitted to ICML 2024 (not accepted)")));
        assert!(!has_accept_cue(Some("This paper hasn\u{2019}t been accepted for publication")));
        assert!(has_accept_cue(Some("Not accepted to ICML 2024 but accepted to ICLR 2025")));
        assert!(!has_accept_cue(Some("This paper has not been accepted for publication.")));
        assert!(has_accept_cue(Some("Rejected earlier. Accepted to FOCS")));
        assert!(!has_accept_cue(None));
    }
}
