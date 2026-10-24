//! Accent-insensitive fuzzy matching: normalisation, Jaro-Winkler, edit-distance ratio,
//! token-window containment and author-name matching.

/// Replacement for a lowercase non-ASCII Latin letter (diacritics stripped, common transliterations).
fn latin(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ǎ' | 'ǻ' | 'ª' => "a",
        'æ' | 'ǽ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'þ' => "th",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' | 'ǐ' => "i",
        'ĳ' => "ij",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'ǒ' | 'ǿ' | 'ơ' | 'º' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ǔ' | 'ư' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'ﬀ' => "ff",
        'ﬁ' => "fi",
        'ﬂ' => "fl",
        '\u{1EA0}'..='\u{1EB7}' => "a",
        '\u{1EB8}'..='\u{1EC7}' => "e",
        '\u{1EC8}'..='\u{1ECB}' => "i",
        '\u{1ECC}'..='\u{1EE3}' => "o",
        '\u{1EE4}'..='\u{1EF1}' => "u",
        '\u{1EF2}'..='\u{1EF9}' => "y",
        _ => return None,
    })
}

/// Lowercase, strip diacritics (Latin incl. ő ł ø ß æ đ ı), non-alnum -> space, collapse whitespace.
/// Apostrophes are dropped (O'Brien -> obrien); other CJK/Cyrillic/Greek letters are kept lowercased.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    let put = |out: &mut String, gap: &mut bool, t: &str| {
        if *gap && !out.is_empty() {
            out.push(' ');
        }
        *gap = false;
        out.push_str(t);
    };
    let mut buf = [0u8; 4];
    for c in s.chars() {
        if c.is_ascii() {
            if c.is_ascii_alphanumeric() {
                put(&mut out, &mut gap, c.to_ascii_lowercase().encode_utf8(&mut buf));
            } else if c != '\'' && c != '`' {
                gap = true;
            }
            continue;
        }
        for l in c.to_lowercase() {
            if ('\u{300}'..='\u{36f}').contains(&l) || l == '’' || l == 'ʼ' || l == '‘' {
                continue;
            }
            if let Some(r) = latin(l) {
                put(&mut out, &mut gap, r);
            } else if l.is_alphanumeric() {
                put(&mut out, &mut gap, l.encode_utf8(&mut buf));
            } else {
                gap = true;
            }
        }
    }
    out
}

const MAXC: usize = 128;

fn load(s: &str, buf: &mut [char; MAXC]) -> usize {
    let mut n = 0;
    for c in s.chars().take(MAXC) {
        buf[n] = c;
        n += 1;
    }
    n
}

fn jaro(a: &[char], b: &[char]) -> f32 {
    let (la, lb) = (a.len(), b.len());
    if la == 0 || lb == 0 {
        return 0.0;
    }
    let w = (la.max(lb) / 2).saturating_sub(1);
    let (mut am, mut bm) = (0u128, 0u128);
    let mut m = 0usize;
    for i in 0..la {
        let hi = (i + w + 1).min(lb);
        for j in i.saturating_sub(w)..hi {
            if bm >> j & 1 == 0 && a[i] == b[j] {
                am |= 1 << i;
                bm |= 1 << j;
                m += 1;
                break;
            }
        }
    }
    if m == 0 {
        return 0.0;
    }
    let (mut j, mut half_t) = (0usize, 0usize);
    for i in 0..la {
        if am >> i & 1 == 1 {
            while bm >> j & 1 == 0 {
                j += 1;
            }
            if a[i] != b[j] {
                half_t += 1;
            }
            j += 1;
        }
    }
    let m = m as f32;
    (m / la as f32 + m / lb as f32 + (m - (half_t / 2) as f32) / m) / 3.0
}

/// Jaro-Winkler similarity 0..1 (compares the first 128 chars).
pub fn jw(a: &str, b: &str) -> f32 {
    if a == b {
        return if a.is_empty() { 0.0 } else { 1.0 };
    }
    let (mut ba, mut bb) = (['\0'; MAXC], ['\0'; MAXC]);
    let (la, lb) = (load(a, &mut ba), load(b, &mut bb));
    let j = jaro(&ba[..la], &bb[..lb]);
    if j <= 0.7 {
        return j;
    }
    let l = ba[..la].iter().zip(&bb[..lb]).take(4).take_while(|(x, y)| x == y).count();
    j + l as f32 * 0.1 * (1.0 - j)
}

/// Normalised edit-distance similarity 0..1 (optimal-string-alignment Damerau-Levenshtein).
pub fn ratio(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let (mut ba, mut bb) = (['\0'; MAXC], ['\0'; MAXC]);
    let (la, lb) = (load(a, &mut ba), load(b, &mut bb));
    if la == 0 || lb == 0 {
        return 0.0;
    }
    let (a, b) = (&ba[..la], &bb[..lb]);
    let mut p2 = [0u16; MAXC + 1];
    let mut p1 = [0u16; MAXC + 1];
    let mut cur = [0u16; MAXC + 1];
    for (j, v) in p1.iter_mut().enumerate().take(lb + 1) {
        *v = j as u16;
    }
    for i in 1..=la {
        cur[0] = i as u16;
        for j in 1..=lb {
            let cost = u16::from(a[i - 1] != b[j - 1]);
            let mut d = (p1[j] + 1).min(cur[j - 1] + 1).min(p1[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(p2[j - 2] + 1);
            }
            cur[j] = d;
        }
        std::mem::swap(&mut p2, &mut p1);
        std::mem::swap(&mut p1, &mut cur);
    }
    1.0 - p1[lb] as f32 / la.max(lb) as f32
}

/// Token similarity: edit ratio, with abbreviation credit (prefix >=3 chars, single initial).
fn tsim(a: &str, b: &str) -> f32 {
    tsim_x(a, b, true)
}

/// `initials`: let a single letter match any word starting with it (names only, never institutions).
fn tsim_x(a: &str, b: &str, initials: bool) -> f32 {
    if a == b {
        return 1.0;
    }
    let mut s = ratio(a, b);
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let (ls, ll) = (short.chars().count(), long.chars().count());
    // abbreviation credit ("univ" -> "university") only for short stems: "prince" must not match "princeton"
    if (3..=5).contains(&ls) && ls * 10 >= ll * 3 && long.starts_with(short) {
        s = s.max(0.85 + 0.15 * ls as f32 / ll as f32);
    } else if initials && ls == 1 && long.starts_with(short) {
        s = s.max(0.88);
    }
    s
}

fn alias(t: &str) -> Option<&'static str> {
    Some(match t {
        "univ" | "uni" | "universitat" | "universite" | "universidad" | "universita" | "universitet"
        | "universiteit" | "universidade" => "university",
        "inst" | "institut" | "instituto" | "istituto" => "institute",
        "dept" | "dep" | "departement" | "departamento" => "department",
        "tech" => "technology",
        "lab" => "laboratory",
        "labs" => "laboratories",
        "natl" => "national",
        "intl" => "international",
        "coll" => "college",
        "sch" => "school",
        "ctr" | "centre" | "zentrum" => "center",
        "math" | "maths" => "mathematics",
        "sci" => "science",
        "caltech" => "california institute technology",
        "mit" => "massachusetts institute technology",
        // well-known acronyms with a unique expansion (generic initials would let UCL match "UC Los ...")
        "ucl" => "university college london",
        "ucla" => "university california los angeles",
        "ucsd" => "university california san diego",
        "ucsb" => "university california santa barbara",
        "ucsf" => "university california san francisco",
        "cmu" => "carnegie mellon university",
        "nyu" => "new york university",
        "usc" => "university southern california",
        "kcl" => "king college london",
        "eth" => "eidgenossische technische hochschule",
        "epfl" => "ecole polytechnique federale lausanne",
        _ => return None,
    })
}

fn is_stop(t: &str) -> bool {
    matches!(
        t,
        "of" | "the" | "at" | "in" | "for" | "and" | "de" | "des" | "du" | "la" | "le" | "der" | "die" | "das"
            | "und" | "et" | "y" | "fur" | "del" | "di"
    )
}

fn tokens<'a>(folded: &'a str, stop: bool, out: &mut Vec<&'a str>) {
    for t in folded.split(' ') {
        if let Some(exp) = alias(t) {
            out.extend(exp.split(' '));
        } else if !(stop && is_stop(t)) {
            out.push(t);
        }
    }
}

fn toks<'a>(folded: &'a str) -> Vec<&'a str> {
    let mut v = Vec::new();
    tokens(folded, true, &mut v);
    if v.is_empty() {
        tokens(folded, false, &mut v);
    }
    v
}

/// Best fuzzy score of `needle` as a (token-aligned) substring of `hay`; both folded internally.
/// Handles abbreviations ("Univ." / "Inst."), word reordering inside a window and acronyms (MIT, ETH).
pub fn contains_fuzzy(hay: &str, needle: &str) -> f32 {
    contains_impl(hay, needle, true)
}

/// Like `contains_fuzzy` but single-letter words in `hay` (middle initials, "P. R. China") never match.
pub fn contains_fuzzy_strict(hay: &str, needle: &str) -> f32 {
    contains_impl(hay, needle, false)
}

fn contains_impl(hay: &str, needle: &str, initials: bool) -> f32 {
    // a one-word needle must not match a lone initial in the haystack ("Annals" vs "Phys. Rev. A")
    let initials = initials && needle.split_whitespace().nth(1).is_some();
    let (fh, fnd) = (fold(hay), fold(needle));
    if fh.is_empty() || fnd.is_empty() {
        return 0.0;
    }
    let acronym_needle = fnd.split(' ').any(|t| alias(t).is_some_and(|e| e.contains(' ')));
    let (ht, nt) = (toks(&fh), toks(&fnd));
    let (h, n) = (ht.len(), nt.len());
    let mut best = 0.0f32;
    for start in 0..=h.saturating_sub(n) {
        let win = &ht[start..(start + n).min(h)];
        let (mut ord, mut bag) = (0.0, 0.0);
        for (i, nw) in nt.iter().enumerate() {
            if let Some(w) = win.get(i) {
                ord += tsim_x(w, nw, initials);
            }
            bag += win.iter().map(|w| tsim_x(w, nw, initials)).fold(0.0, f32::max);
        }
        best = best.max(ord / n as f32);
        if !acronym_needle {
            best = best.max(0.97 * bag / n as f32); // unordered match: not for acronym expansions (NYU != "State University of New York")
        }
        if best >= 1.0 {
            return 1.0;
        }
    }
    let k = fnd.chars().count();
    // generic initials matching only for acronyms without an explicit expansion ("MIT" has one)
    if !fnd.contains(' ') && (2..=6).contains(&k) && fnd.is_ascii() && k <= h && alias(&fnd).is_none() {
        for win in ht.windows(k) {
            if win.iter().zip(fnd.bytes()).all(|(w, c)| w.as_bytes()[0] == c) {
                best = best.max(0.97);
                break;
            }
        }
    }
    best
}

const PARTICLES: &[&str] = &[
    "van", "von", "der", "den", "de", "del", "della", "di", "da", "dos", "das", "du", "la", "le", "al", "el",
    "bin", "ibn", "ter", "ten", "zu", "af", "des", "dei", "degli",
];

fn is_particle(t: &str) -> bool {
    PARTICLES.contains(&t)
}

/// Parsed person name: folded given-name tokens and folded surname core (particles removed).
#[derive(Clone, Debug, Default)]
struct Name {
    given: Vec<String>,
    core: Vec<String>,
}

fn strip_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn fold_split(s: &str) -> impl Iterator<Item = String> {
    fold(s).split(' ').filter(|t| !t.is_empty()).map(str::to_string).collect::<Vec<_>>().into_iter()
}

fn core_of(raw: &str) -> Vec<String> {
    let all: Vec<String> = fold_split(raw).collect();
    let core: Vec<String> = all.iter().filter(|t| !is_particle(t)).cloned().collect();
    if core.is_empty() {
        all
    } else {
        core
    }
}

/// Push given-name tokens of `raw` (all-caps "TJ" becomes initials unless `keep_caps`).
fn push_given(raw: &str, expand_caps: bool, out: &mut Vec<String>) {
    // packed dotted initials ("Yu.A.") become separate runs ("Yu.", "A.")
    if raw.matches('.').count() >= 2 && !raw.contains('-') {
        for piece in raw.split_inclusive('.').filter(|p| p.chars().any(char::is_alphabetic)) {
            push_given(piece, expand_caps, out);
        }
        return;
    }
    let b = raw.as_bytes();
    if expand_caps && (2..=3).contains(&b.len()) && b.iter().all(u8::is_ascii_uppercase) {
        out.extend(raw.chars().map(|c| c.to_ascii_lowercase().to_string()));
    } else if raw.ends_with('.') && raw.chars().filter(|c| c.is_alphabetic()).count() == 2 && !raw.contains('-') {
        // transliterated initial ("Yu.", "Ph."): keep the dot so it matches any name starting with it
        out.extend(fold_split(raw).map(|t| format!("{t}.")));
    } else {
        out.extend(fold_split(raw));
    }
}

fn parse_name(raw: &str) -> Name {
    let mut clean = strip_brackets(raw);
    // "John Smith, Jr." / "Smith, John, Jr.": drop trailing generational suffixes before detecting name order
    while let Some((head, tail)) = clean.rsplit_once(',') {
        if matches!(fold(tail).as_str(), "jr" | "sr" | "ii" | "iii" | "iv") {
            clean = head.to_string();
        } else {
            break;
        }
    }
    let mut n = Name::default();
    if let Some((sur, rest)) = clean.split_once(',') {
        n.core = core_of(sur);
        for t in rest.split(',').next().unwrap_or("").split_whitespace() {
            push_given(t, true, &mut n.given);
        }
        while n.given.last().is_some_and(|t| is_particle(t)) {
            n.given.pop();
        }
        return n;
    }
    let mut toks: Vec<&str> = clean.split_whitespace().collect();
    while toks.len() > 1 && matches!(fold(toks[toks.len() - 1]).as_str(), "jr" | "sr" | "ii" | "iii" | "iv") {
        toks.pop();
    }
    // "Smirnov A. Yu.": one full word followed only by dotted initials is written surname-first
    if toks.len() >= 2 && !crate::out::is_initials(toks[0]) && toks[1..].iter().all(|t| crate::out::is_initials(t)) {
        n.core = core_of(toks[0]);
        for t in &toks[1..] {
            push_given(t, true, &mut n.given);
        }
        return n;
    }
    let Some(last) = toks.pop() else { return n };
    n.core = core_of(last);
    while toks.last().is_some_and(|t| is_particle(&fold(t)) || t.starts_with(['\'', '\u{2019}'])) {
        toks.pop();
    }
    let caps = !last.bytes().all(|b| b.is_ascii_uppercase() || !b.is_ascii_alphabetic());
    for t in toks {
        push_given(t, caps, &mut n.given);
    }
    n
}

fn swapped_name(raw: &str) -> Option<Name> {
    let clean = strip_brackets(raw);
    if clean.contains(',') {
        return None;
    }
    let mut it = clean.split_whitespace();
    let first = it.next()?;
    let mut n = Name { core: core_of(first), given: Vec::new() };
    for t in it {
        push_given(t, false, &mut n.given);
    }
    (!n.given.is_empty()).then_some(n)
}

fn surname_sim(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let j = jw(a, b);
    if a.chars().count().max(b.chars().count()) <= 4 || ratio(a, b) < 0.75 {
        j.min(0.6)
    } else {
        j
    }
}

const SURNAME_GATE: f32 = 0.88;

fn surnames_sim(q: &[String], a: &[String]) -> f32 {
    let (Some(ql), Some(al)) = (q.last(), a.last()) else { return 0.0 };
    if q.len() > 1 && a.len() > 1 {
        // both sides give a compound surname: every component must agree ("Pérez-García" != "López-García")
        return if q.len() == a.len() {
            q.iter().zip(a).map(|(x, y)| surname_sim(x, y)).fold(1.0, f32::min)
        } else {
            surname_sim(&q.join(" "), &a.join(" "))
        };
    }
    let mut s = surname_sim(ql, al);
    if q.len() > 1 && a.len() == 1 {
        // the query names several surname components but the author lists one: plausible (dropped
        // second surname) yet not exact
        s *= 0.9;
    }
    if q.len() > 1 || a.len() > 1 {
        s = s.max(surname_sim(&q.join(" "), &a.join(" ")));
        if q.len() == 1 || a.len() == 1 {
            for x in q {
                for y in a {
                    s = s.max(0.97 * surname_sim(x, y));
                }
            }
        }
    }
    s
}

/// The letters an initial stands for: a single letter ("t") or a dotted transliterated initial ("yu.").
fn init_prefix(t: &str) -> Option<&str> {
    if t.chars().count() == 1 {
        Some(t)
    } else {
        t.strip_suffix('.')
    }
}

fn given_factor(q: &[String], a: &[String]) -> f32 {
    let Some(qf) = q.first() else { return 0.9 };
    let Some(af) = a.first() else { return 0.86 };
    let (qc, ac) = (qf.chars().next(), af.chars().next());
    let (qp, ap) = (init_prefix(qf), init_prefix(af));
    let mut f = if qf == af {
        1.0
    } else if qp.is_some() || ap.is_some() {
        let (qn, an) = (qf.trim_end_matches('.'), af.trim_end_matches('.'));
        let compat = match (qp, ap) {
            (Some(_), Some(_)) => qn.starts_with(an) || an.starts_with(qn),
            (Some(p), None) => an.starts_with(p),
            (None, Some(p)) => qn.starts_with(p),
            (None, None) => false,
        };
        if compat { 0.92 } else { 0.3 }
    } else if jw(qf, af) >= 0.88 || tsim(qf, af) >= 0.88 {
        0.95
    } else if qc == ac {
        0.45
    } else {
        0.3
    };
    // every further supplied given-name token must agree too (middle names / initials)
    for (qt, at) in q.iter().zip(a.iter()).skip(1) {
        let (qp, ap) = (init_prefix(qt), init_prefix(at));
        let same = if qp.is_some() || ap.is_some() {
            let (qn, an) = (qt.trim_end_matches('.'), at.trim_end_matches('.'));
            qn.starts_with(an) || an.starts_with(qn)
        } else {
            qt == at || jw(qt, at) >= 0.88
        };
        if !same {
            f *= 0.8;
        }
    }
    f
}

fn direct(q: &Name, a: &Name) -> f32 {
    let s = surnames_sim(&q.core, &a.core);
    if s >= SURNAME_GATE {
        (s * given_factor(&q.given, &a.given)).min(1.0)
    } else {
        s * 0.4
    }
}

const COLLAB_WORDS: &[&str] = &["collaboration", "collaborations", "consortium", "experiment", "working", "team", "project"];

/// Some(score) when `author` is a collaboration: every query token must appear among its words.
fn collab_score(q: &Name, author: &str) -> Option<f32> {
    let words: Vec<String> = fold_split(&strip_brackets(author)).collect();
    if !words.iter().any(|w| COLLAB_WORDS.contains(&w.as_str())) {
        return None;
    }
    let qt: Vec<&String> = q.given.iter().chain(q.core.iter()).collect();
    if qt.is_empty() {
        return Some(0.0);
    }
    let all = qt.iter().all(|t| words.iter().any(|w| surname_sim(t, w) >= SURNAME_GATE));
    Some(if all { 0.95 } else { 0.0 })
}

/// Author-name query ("Terence Tao", "Tao, T.", "van der Waerden", "Erdős").
pub struct AuthorQ {
    name: Name,
    /// All-surname reading of a space-separated query ("Garcia Ferrero" = compound surname), tried too.
    compound: Option<Name>,
}
impl AuthorQ {
    pub fn new(q: &str) -> AuthorQ {
        let name = parse_name(q);
        let folded: Vec<String> = fold_split(&strip_brackets(q)).collect();
        let compound = (!q.contains(',') && folded.len() >= 2 && folded.iter().all(|t| t.chars().count() >= 2))
            .then(|| Name { given: Vec::new(), core: folded });
        AuthorQ { name, compound }
    }
    /// 0..1: how well a paper author string matches the query name.
    pub fn score(&self, author: &str) -> f32 {
        if self.name.core.is_empty() {
            return 0.0;
        }
        // collaborations ("The ATLAS Collaboration"): match organisation words, not a personal surname
        if let Some(c) = collab_score(&self.name, author) {
            return c;
        }
        let a = parse_name(author);
        let s = direct(&self.name, &a);
        let s = match &self.compound {
            // only against authors whose own surname is as long: "Terence Tao" must not read as the
            // compound surname "Terence Tao" and then match "Sarah Tao"
            Some(c) if a.core.len() >= c.core.len() => s.max(direct(c, &a)),
            // spaced compound surname written in full ("María Ángeles García Ferrero"): trailing name tokens
            Some(c) => {
                let at: Vec<String> = fold_split(&strip_brackets(author)).collect();
                let k = c.core.len();
                if at.len() >= k && at[at.len() - k..].iter().zip(&c.core).all(|(x, y)| surname_sim(x, y) >= SURNAME_GATE) {
                    s.max(0.92)
                } else {
                    s
                }
            }
            _ => s,
        };
        if s < 0.5 && !self.name.given.is_empty() {
            if let Some(sw) = swapped_name(author) {
                return s.max(0.92 * direct(&self.name, &sw));
            }
        }
        s
    }
    fn token(&self) -> String {
        let mut best = String::new();
        for t in &self.name.core {
            let a: String = t.chars().filter(|c| c.is_alphanumeric()).collect();
            if a.chars().count() >= best.chars().count() {
                best = a;
            }
        }
        best
    }
    /// Safe ASCII token for the arXiv `au:` field (surname-ish).
    pub fn api_token(&self) -> String {
        self.token()
    }
    /// Looser API token (3+ char prefix + `*`) for typo fallback; None if too short.
    pub fn api_wild(&self) -> Option<String> {
        let t = self.token();
        let n = t.chars().count();
        (n >= 5).then(|| format!("{}*", t.chars().take((n - 2).clamp(3, 5)).collect::<String>()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_cases() {
        assert_eq!(fold("Erdős, Paul"), "erdos paul");
        assert_eq!(fold("  Łukasiewicz–Søren  "), "lukasiewicz soren");
        assert_eq!(fold("Weiß Æsop Œuvre Đ ı"), "weiss aesop oeuvre d i");
        assert_eq!(fold("Nguyễn Văn"), "nguyen van");
        assert_eq!(fold("e\u{301}cole"), "ecole");
        assert_eq!(fold("O'Brien D’Angelo"), "obrien dangelo");
        assert_eq!(fold("東京大学 Москва"), "東京大学 москва");
        assert_eq!(fold("Çağlar Ñandú Ünal"), "caglar nandu unal");
        assert_eq!(fold("---"), "");
        assert_eq!(fold(""), "");
    }

    #[test]
    fn jw_ratio_values() {
        assert!((jw("martha", "marhta") - 0.9611).abs() < 1e-3);
        assert!((jw("dixon", "dicksonx") - 0.8133).abs() < 1e-3);
        assert_eq!(jw("abc", "abc"), 1.0);
        assert_eq!(jw("abc", "xyz"), 0.0);
        assert_eq!(jw("", "abc"), 0.0);
        assert!((ratio("kitten", "sitting") - (1.0 - 3.0 / 7.0)).abs() < 1e-6);
        assert!((ratio("ab", "ba") - 0.5).abs() < 1e-6);
        assert_eq!(ratio("same", "same"), 1.0);
        assert_eq!(ratio("", "x"), 0.0);
        let long = "a".repeat(500);
        assert!(ratio(&long, &long[..300]) > 0.0);
        assert!(jw(&long, &long[..300]) > 0.9);
    }

    #[test]
    fn contains_inst_cases() {
        let ok = |h: &str, n: &str| {
            let s = contains_fuzzy(h, n);
            assert!(s >= 0.82, "{h:?} ~ {n:?} = {s}");
        };
        let no = |h: &str, n: &str| {
            let s = contains_fuzzy(h, n);
            assert!(s < 0.82, "{h:?} !~ {n:?} = {s}");
        };
        ok("Massachusetts Institute of Technology", "MIT");
        ok("Dept. of Math, Massachusetts Inst. of Tech.", "Massachusetts Institute of Technology");
        ok("Univ. of Cambridge", "University of Cambridge");
        ok("University of Cambridge, UK", "Cambridge");
        ok("Cambridge University", "University of Cambridge");
        ok("Universität Bonn", "University of Bonn");
        ok("Eidgenössische Technische Hochschule Zürich", "ETH");
        ok("Max Planck Institute for Mathematics", "MPIM");
        ok("Stanford University", "Stanfrod University");
        ok("Technische Universität München", "Technische Universitat Munchen");
        ok("Department of Mathematics, UCLA", "UCLA");
        ok("Caltech", "California Institute of Technology");
        ok("Ecole Normale Supérieure", "Ecole Normale Superieure");
        ok("東京大学 理学部", "東京大学");
        no("Michigan State University", "University of Michigan");
        no("University of Oxford", "University of Cambridge");
        no("Massachusetts General Hospital", "MIT");
        no("Stanford University", "Harvard University");
        no("Mathematical Institute, Oxford", "Institute for Advanced Study");
        no("", "MIT");
        no("MIT", "");
    }

    #[test]
    fn contains_jref_cases() {
        assert!(contains_fuzzy("Ann. of Math. 198 (2023) 1-50", "Annals of Mathematics") >= 0.85);
        assert!(contains_fuzzy("J. Combin. Theory Ser. A 12 (2020)", "Journal of Combinatorial Theory Series A") >= 0.85);
        assert!(contains_fuzzy("Duke Math. J. 5 (1939)", "Annals of Mathematics") < 0.85);
    }

    #[test]
    fn author_table() {
        let yes: &[(&str, &str)] = &[
            ("Terence Tao", "Terence Tao"),
            ("Terence Tao", "Tao, Terence"),
            ("Tao, Terence", "Terence Tao"),
            ("Terence Tao", "T. Tao"),
            ("T. Tao", "Terence Tao"),
            ("T. Tao", "Tao, T."),
            ("tao", "Terence Tao"),
            ("tao", "Tao, T."),
            ("Tao", "Terence Chi-Shen Tao"),
            ("Erdős", "Paul Erdos"),
            ("Paul Erdos", "P. Erdős"),
            ("Erdos, P.", "Paul Erdős"),
            ("van der Waerden", "B. L. van der Waerden"),
            ("Waerden", "Waerden, B. L. van der"),
            ("van der Waerden", "Waerden, B.L. van der"),
            ("de Bruijn", "Nicolaas Govert de Bruijn"),
            ("Bruijn", "N. G. de Bruijn"),
            ("Ben Green", "Ben Green"),
            ("Ben Green", "Benjamin Green"),
            ("Ben Green", "B. J. Green"),
            ("Ben Green", "Ben Joseph Green"),
            ("Terence Tao", "Terrence Tao"),
            ("Terrence Tao", "Terence Tao"),
            ("Terence Tao", "Terence Tao (UCLA)"),
            ("Tao", "Terence Tao Jr."),
            ("Müller", "Mueller"),
            ("Jean-Pierre Serre", "J.-P. Serre"),
            ("Jean-Pierre Serre", "Serre, Jean-Pierre"),
            ("Wei Zhang", "Zhang Wei"),
            ("Nguyen", "Minh Nguyễn"),
            ("O'Brien", "Ryan OBrien"),
            ("Szemerédi", "Endre Szemeredi"),
            ("Łaba", "Izabella Laba"),
            ("Gowers", "W. T. Gowers"),
            ("Waerdn", "van der Waerden"),
            ("Тао", "Терренс Тао"),
        ];
        for (q, a) in yes {
            let s = AuthorQ::new(q).score(a);
            assert!(s >= 0.82, "{q:?} vs {a:?} = {s}");
        }
        let no: &[(&str, &str)] = &[
            ("T. Tao", "S. Tao"),
            ("Terence Tao", "Sarah Tao"),
            ("Terence Tao", "Thomas Tao"),
            ("Terence Tao", "Terence Taylor"),
            ("Tao", "Tan"),
            ("Li", "Liu"),
            ("Chen", "Chan"),
            ("Ben Green", "Ben Greenfield"),
            ("Ben J. Green", "Ben L. Green"),
            ("Tao", "Tao Li"),
            ("van der Waerden", "Hans Haarlem"),
            ("", "Terence Tao"),
            ("Tao", ""),
        ];
        for (q, a) in no {
            let s = AuthorQ::new(q).score(a);
            assert!(s < 0.82, "{q:?} vs {a:?} = {s}");
        }
    }

    #[test]
    fn author_score_values() {
        let s = |q: &str, a: &str| AuthorQ::new(q).score(a);
        assert_eq!(s("Terence Tao", "Terence Tao"), 1.0);
        assert!((s("tao", "Terence Tao") - 0.9).abs() < 1e-6);
        assert!(s("T. Tao", "Terence Tao") >= 0.9);
        assert!(s("S. Tao", "Terence Tao") < 0.5);
        assert!(s("Alex Tao", "Alexander Tao") > 0.9);
        assert!(s("Terence Tao", "Tao") > 0.82);
    }

    #[test]
    fn dropped_second_surname_is_not_exact() {
        let s = AuthorQ::new("Juan Pérez-García").score("Juan López García");
        assert!(s < 1.0 && s >= 0.82, "{s}");
        assert!(AuthorQ::new("García").score("Juan Pérez-García") >= 0.9);
    }

    #[test]
    fn full_middle_names_must_agree() {
        assert!(AuthorQ::new("Pham Duc Tao").score("Pham Dinh Tao") < 0.82);
        assert!(AuthorQ::new("Pham Duc Tao").score("Pham D. Tao") >= 0.9);
        assert!(AuthorQ::new("Pham Duc Tao").score("Pham Duc Tao") >= 0.95);
    }

    #[test]
    fn every_given_initial_counts() {
        assert!(AuthorQ::new("A. B. C. Wu").score("A. B. D. Wu") < 0.82);
        assert!(AuthorQ::new("A. B. C. Wu").score("A. B. C. Wu") >= 0.95);
        assert!(AuthorQ::new("A. Wu").score("A. B. D. Wu") >= 0.9);
    }

    #[test]
    fn packed_transliterated_initials() {
        assert!(AuthorQ::new("Yuri Simonov").score("Yu.A. Simonov") >= 0.9);
        assert!(AuthorQ::new("Yuri A. Simonov").score("Yu.A. Simonov") >= 0.9);
        assert!(AuthorQ::new("Anna B. Simonov").score("Yu.A. Simonov") < 0.82);
    }

    #[test]
    fn transliterated_initials_match_full_names() {
        assert!(AuthorQ::new("Yu. Smirnov").score("Yuri Smirnov") >= 0.9);
        assert!(AuthorQ::new("Yuri Smirnov").score("Yu. Smirnov") >= 0.9);
        assert!(AuthorQ::new("Yu. Smirnov").score("Anna Smirnov") < 0.5);
        assert!(AuthorQ::new("Yu Smirnov").score("Yuri Smirnov") >= 0.4);
    }

    #[test]
    fn surname_first_with_trailing_initials_matches() {
        assert!(AuthorQ::new("Smirnov").score("Smirnov A. Yu.") >= 0.9);
        assert!(AuthorQ::new("A. Smirnov").score("Smirnov A. Yu.") >= 0.9);
        assert!(AuthorQ::new("B. Smirnov").score("Smirnov A. Yu.") < 0.82);
    }

    #[test]
    fn collaborations_match_by_organisation_name() {
        assert!(AuthorQ::new("ATLAS").score("The ATLAS Collaboration") >= 0.9);
        assert!(AuthorQ::new("CMS").score("The ATLAS Collaboration") < 0.5);
        assert!(AuthorQ::new("Tao").score("The ATLAS Collaboration") < 0.5);
    }

    #[test]
    fn surname_first_with_initials_still_passes() {
        assert!(AuthorQ::new("Y.-M. Chen").score("Chen Yu-Ming") >= 0.82);
        assert!(AuthorQ::new("Yu-Ming Chen").score("Chen Yu-Ming") >= 0.82);
        assert!(AuthorQ::new("Y.-M. Chen").score("Chen Hao") < 0.5);
    }

    #[test]
    fn compound_surname_components_must_agree() {
        assert!(AuthorQ::new("Juan Pérez-García").score("Juan López-García") < 0.5);
        assert!(AuthorQ::new("Juan Pérez-García").score("Juan Pérez-García") >= 0.95);
        assert!(AuthorQ::new("Garcia Ferrero").score("María Ángeles García Ferrero") >= 0.9);
        assert!(AuthorQ::new("Terence Tao").score("Sarah Tao") < 0.82);
    }

    #[test]
    fn space_and_hyphen_compound_surnames_agree() {
        assert!(AuthorQ::new("Garcia Ferrero").score("María-Ángeles García-Ferrero") >= 0.9);
        assert!(AuthorQ::new("Terence Tao").score("Terence Tao") >= 0.9);
        assert!(AuthorQ::new("Wei Li").score("Hui Li") < 0.5);
    }

    #[test]
    fn apostrophe_particle_surnames() {
        let q = AuthorQ::new("'t Hooft");
        assert!(q.score("Gerard 't Hooft") >= 0.9);
        assert!(q.score("Gerard Hooft") >= 0.9);
        assert!(AuthorQ::new("G. Hooft").score("Gerard Hooft") >= 0.9);
    }

    #[test]
    fn comma_suffixes_do_not_flip_name_order() {
        let q = AuthorQ::new("John Smith");
        assert!(q.score("John Smith, Jr.") >= 0.9);
        assert!(q.score("Smith, John, Jr.") >= 0.9);
        assert!(q.score("John Smith") >= 0.9);
    }

    #[test]
    fn api_tokens() {
        let t = |q: &str| (AuthorQ::new(q).api_token(), AuthorQ::new(q).api_wild());
        assert_eq!(t("Terence Tao"), ("tao".into(), None));
        assert_eq!(t("Erdős"), ("erdos".into(), Some("erd*".into())));
        assert_eq!(t("van der Waerden"), ("waerden".into(), Some("waerd*".into())));
        assert_eq!(t("Tao, Terence"), ("tao".into(), None));
        assert_eq!(t("Szemerédi, E."), ("szemeredi".into(), Some("szeme*".into())));
        assert_eq!(t("Hardy-Littlewood"), ("littlewood".into(), Some("littl*".into())));
        assert_eq!(t("Greene"), ("greene".into(), Some("gree*".into())));
        assert_eq!(t("東京"), ("東京".into(), None));
        assert_eq!(t("张伟"), ("张伟".into(), None));
    }
}

#[cfg(test)]
mod inst_regress {
    use super::*;
    #[test]
    fn single_word_needle_ignores_series_letters() {
        assert!(contains_fuzzy("Phys. Rev. A 105, 022411 (2022)", "Annals") < 0.85);
        assert!(contains_fuzzy("J. Combin. Theory Ser. A 12 (2020)", "Journal of Combinatorial Theory Series A") > 0.85);
    }

    #[test]
    fn acronym_plus_city_queries() {
        assert!(contains_fuzzy_strict("Eidgenössische Technische Hochschule Zürich", "ETH Zurich") > 0.9);
        assert!(contains_fuzzy_strict("ETH Zurich, Switzerland", "ETH Zurich") > 0.9);
        assert!(contains_fuzzy_strict("École Polytechnique Fédérale de Lausanne", "EPFL") > 0.9);
    }

    #[test]
    fn nyu_is_not_suny() {
        assert!(contains_fuzzy_strict("State University of New York at Stony Brook", "NYU") < 0.8);
        assert!(contains_fuzzy_strict("New York University, Courant Institute", "NYU") > 0.9);
    }

    #[test]
    fn ucl_is_not_ucla() {
        assert!(contains_fuzzy_strict("University of California, Los Angeles", "UCL") < 0.8);
        assert!(contains_fuzzy_strict("University College London", "UCL") > 0.9);
        assert!(contains_fuzzy_strict("University of California, Los Angeles", "UCLA") > 0.9);
    }

    #[test]
    fn explicit_acronym_alias_beats_initials() {
        assert!(contains_fuzzy_strict("Mathematical Institute, Tohoku University", "MIT") < 0.8);
        assert!(contains_fuzzy_strict("Massachusetts Institute of Technology", "MIT") > 0.9);
        assert!(contains_fuzzy_strict("Eidgenossische Technische Hochschule Zurich", "ETH") > 0.9);
    }

    #[test]
    fn long_prefix_is_not_an_abbreviation() {
        assert!(contains_fuzzy_strict("University of Northern British Columbia, Prince George", "princeton") < 0.8);
        assert!(contains_fuzzy_strict("Univ. of Cambridge", "university of cambridge") > 0.9);
    }

    #[test]
    fn initial_in_haystack_is_not_a_match() {
        assert!(contains_fuzzy_strict("Qiuzhen College, Tsinghua University, Beijing, P. R. China", "princeton") < 0.5);
        assert!(contains_fuzzy_strict("Department of Mathematics, Princeton University, NJ", "princeton") > 0.99);
    }
}
