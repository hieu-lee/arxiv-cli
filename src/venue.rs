//! Venue (journal/conference) quartile resolution from journal_ref / comment text.
//!
//! Text is folded into canonical tokens (lowercase, diacritics stripped, punctuation -> space,
//! stopwords dropped, common abbreviations unified), then matched as token sequences against an
//! index keyed by first token. Quartiles are approximate (see data/journals.tsv header).

use crate::model::Venue;
use std::fmt::Write;
use std::sync::{OnceLock, RwLock};

static DATA: &str = include_str!("../data/journals.tsv");

const F_UPPER: u8 = 1;
const F_CAP: u8 = 2;
const F_NUM: u8 = 4;
const F_YEAR: u8 = 8;
/// Token is followed by a comma or semicolon (a remark may follow, not a title continuation).
const F_BREAK: u8 = 16;

// ---------------------------------------------------------------------------------------------
// Folding
// ---------------------------------------------------------------------------------------------

struct Tok {
    s: u32,
    e: u32,
    flags: u8,
}

#[derive(Default)]
struct Folded {
    buf: String,
    toks: Vec<Tok>,
    /// Token under construction (kept to reuse its capacity across `fold_into` calls).
    cur: String,
}

impl Folded {
    fn text(&self, i: usize) -> &str {
        let t = &self.toks[i];
        &self.buf[t.s as usize..t.e as usize]
    }
    fn len(&self) -> usize {
        self.toks.len()
    }
    fn flags(&self, i: usize) -> u8 {
        self.toks[i].flags
    }
}

fn push_folded(c: char, out: &mut String) {
    if c.is_ascii() {
        out.push(c.to_ascii_lowercase());
        return;
    }
    for lc in c.to_lowercase() {
        let r = match lc {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
            'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => 'c',
            'ď' | 'đ' => 'd',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
            'ĥ' | 'ħ' => 'h',
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
            'ĵ' => 'j',
            'ķ' => 'k',
            'ĺ' | 'ļ' | 'ľ' | 'ł' => 'l',
            'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
            'ŕ' | 'ŗ' | 'ř' => 'r',
            'ś' | 'ŝ' | 'ş' | 'š' => 's',
            'ţ' | 'ť' => 't',
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
            'ŵ' => 'w',
            'ý' | 'ÿ' | 'ŷ' => 'y',
            'ź' | 'ż' | 'ž' => 'z',
            'ß' => {
                out.push_str("ss");
                continue;
            }
            'æ' => {
                out.push_str("ae");
                continue;
            }
            'œ' => {
                out.push_str("oe");
                continue;
            }
            x => x,
        };
        out.push(r);
    }
}

fn is_stop(t: &str) -> bool {
    matches!(
        t,
        "the" | "of" | "and" | "in" | "for" | "on" | "der" | "die" | "das" | "de" | "du" | "des"
            | "und" | "et" | "fur" | "la" | "le" | "its" | "their" | "with" | "zur" | "di" | "ed"
    )
}

/// Unify common abbreviations/variants of a folded token.
fn canon(t: &str) -> &str {
    match t {
        "journals" | "jour" | "jnl" | "journal" => "j",
        "transactions" | "trans" => "trans",
        "proceedings" | "proceeding" | "proc" => "proc",
        "mathematics" | "mathematical" | "mathematische" | "mathematik" | "mathematique"
        | "mathematiques" | "mathematica" | "mathematicae" | "mathematici" | "mathematicum"
        | "mathematicorum" | "matematik" | "matematica" | "matematicas" | "matematiques" | "mat" | "math" => {
            "math"
        }
        "annals" | "annalen" | "annales" | "annali" | "ann" => "ann",
        "combinatorial" | "combinatorics" | "combinatoire" | "combinatoria" | "combin" | "comb" => {
            "comb"
        }
        "communications" | "commun" | "comm" => "comm",
        "commentarii" | "comment" => "comment",
        "applied" | "applications" | "application" | "appl" | "appliquees" | "applicata"
        | "applicatae" | "applicandae" => "appl",
        "american" | "amer" | "am" => "amer",
        "society" | "soc" => "soc",
        "international" | "internat" | "intern" | "int" => "int",
        "analysis" | "analyse" | "anal" => "anal",
        "geometry" | "geom" | "geometric" | "geometrical" | "geometriae" | "geometrie"
        | "geometricorum" => "geom",
        "algebraic" | "algebr" | "algebraique" => "algebr",
        "theory" | "theor" | "theoretical" | "theoretic" | "theoret" | "theorie" => "theor",
        "computer" | "computers" | "computing" | "computation" | "computational" | "comput"
        | "comp" => "comput",
        "complexity" | "complex" => "complex",
        "statistics" | "statistical" | "statistique" | "statistiques" | "statist" | "stat"
        | "stats" => "stat",
        "probability" | "probabilities" | "probabilites" | "probab" | "prob" => "prob",
        "physics" | "physical" | "phys" => "phys",
        "science" | "sciences" | "scientific" | "sci" => "sci",
        "research" | "res" => "res",
        "review" | "reviews" | "rev" => "rev",
        "letters" | "lett" => "lett",
        "discrete" | "discr" => "discrete",
        "electronic" | "electron" | "elec" => "electron",
        "european" | "europ" | "eur" => "european",
        "zeitschrift" => "z",
        "quarterly" | "quart" | "q" => "q",
        "oxford" | "oxf" => "oxford",
        "cambridge" | "camb" => "cambridge",
        "philosophical" | "philos" | "phil" => "philos",
        "london" | "lond" => "london",
        "national" | "natl" => "natl",
        "academy" | "acad" => "acad",
        "institute" | "institut" | "inst" => "inst",
        "publications" | "publ" => "publ",
        "differential" | "differ" | "diff" => "diff",
        "equations" | "equ" | "eq" => "equ",
        "information" | "inform" | "informaticae" | "inf" => "inform",
        "learning" | "learn" => "learn",
        "machine" | "mach" => "mach",
        "intelligence" | "intell" => "intell",
        "artificial" | "artif" => "artif",
        "networks" | "network" | "netw" => "netw",
        "engineering" | "eng" | "engrg" => "eng",
        "english" | "engl" => "engl",
        "mechanics" | "mech" => "mech",
        "numerical" | "numerische" | "numer" => "numer",
        "lineaire" => "linear",
        "symbolic" | "symb" => "symb",
        "topology" | "topol" | "topologie" => "topol",
        "functional" | "funct" => "funct",
        "dynamical" | "dynamics" | "dynam" | "dyn" => "dynam",
        "systems" | "system" | "syst" => "syst",
        "algorithms" | "algorithm" | "algor" => "algor",
        "structures" | "struct" => "struct",
        "processing" | "processes" | "process" => "process",
        "surveys" | "surv" => "surv",
        "reports" | "rep" => "rep",
        "magazine" | "mag" => "mag",
        "education" | "educational" | "educ" => "educ",
        "software" | "softw" => "softw",
        "sequences" | "seq" => "seq",
        "series" | "ser" => "ser",
        "experimental" | "exp" => "exp",
        "modern" | "mod" => "mod",
        "formal" | "form" => "form",
        "logic" | "log" => "logic",
        "categories" | "categ" => "categ",
        "advances" | "adv" => "adv",
        "bulletin" | "bull" => "bull",
        "association" | "assoc" => "assoc",
        "university" | "univ" => "univ",
        "representation" | "representations" | "represent" => "represent",
        "continuous" | "contin" => "contin",
        "contemporary" | "contemp" => "contemp",
        "modelling" | "modeling" | "models" | "model" => "model",
        "problems" | "probl" => "probl",
        "canadian" | "canad" => "canad",
        "australian" | "aust" => "aust",
        "royal" | "roy" => "royal",
        "evolution" | "evol" => "evol",
        "operator" | "operators" | "oper" | "operations" => "oper",
        "harmonic" | "harmon" => "harmon",
        "approximation" | "approx" => "approx",
        "constructive" | "constr" => "constr",
        "arithmetica" | "arith" => "arith",
        "fundamenta" | "fundam" | "fund" => "fund",
        "colloquium" | "colloq" => "colloq",
        "hungarica" | "hungar" => "hungar",
        "sinica" | "sin" => "sin",
        "scandinavica" | "scand" => "scand",
        "edinburgh" | "edinb" => "edinb",
        "section" | "sect" => "sect",
        "indagationes" | "indag" => "indag",
        "monatshefte" | "monatsh" => "monatsh",
        "archiv" | "archive" | "arch" => "arch",
        "rational" | "ration" => "ration",
        "foundations" | "found" => "found",
        "programming" | "program" => "program",
        "recognition" | "recognit" => "recognit",
        "vision" | "vis" => "vis",
        "linguistics" | "linguist" | "linguistic" => "linguist",
        "classical" | "class" => "class",
        "gravity" | "grav" => "grav",
        "nuclear" | "nucl" => "nucl",
        "astronomical" | "astron" => "astron",
        "astrophysics" | "astrophys" => "astrophys",
        "cosmology" | "cosmol" => "cosmol",
        "angewandte" | "angew" => "angew",
        "arkiv" | "ark" => "ark",
        "nachrichten" | "nachr" => "nachr",
        "exposition" | "expositiones" | "exposit" => "exposit",
        "enseignement" | "enseign" => "enseign",
        "faculte" | "fac" => "fac",
        "ecole" | "ec" => "ecole",
        "superieure" | "super" | "sup" => "sup",
        "scientiarum" | "scientiarium" => "sci",
        "studia" | "stud" | "studies" => "stud",
        "inequalities" | "inequal" => "inequal",
        "publicationes" | "publicacions" | "publicaciones" => "publ",
        "cryptography" | "cryptogr" => "crypt",
        "japanese" | "japan" | "jpn" => "jpn",
        "moscow" | "mosc" => "mosc",
        "mediterranean" | "mediterr" => "mediterr",
        "elemente" | "elem" => "elem",
        "gazette" | "gaz" => "gaz",
        "scientia" => "sci",
        "contemporanea" => "contemp",
        "iberoamericana" | "iberoam" => "iberoam",
        "revista" => "rev",
        "helvetici" | "helv" => "helv",
        "memoirs" | "mem" => "mem",
        "logical" => "logic",
        "optimization" | "optim" => "optim",
        "documenta" | "doc" => "doc",
        "australasian" | "australas" => "australas",
        "contributions" | "contrib" => "contrib",
        "seminaire" | "semin" | "sem" => "sem",
        "lotharingien" | "lothar" => "lothar",
        t => t,
    }
}

fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '’' | '‘' | '`' | 'ʼ')
}

/// "COCOON2025" -> "COCOON 2025": a four-digit year glued to a letter run becomes its own token; likewise a
/// series letter glued to its volume ("J.Phys.A39:8933" -> "J.Phys.A 39:8933").
fn split_year_suffix(input: &str) -> std::borrow::Cow<'_, str> {
    let b = input.as_bytes();
    let mut cuts = Vec::new();
    for i in 1..b.len().saturating_sub(2) {
        let series = b[i].is_ascii_uppercase()
            && matches!(b[i - 1], b'.' | b' ' | b',')
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit();
        if series {
            cuts.push(i + 1);
        }
    }
    for i in 2..b.len().saturating_sub(3) {
        let year = (b[i] == b'1' && b[i + 1] == b'9' || b[i] == b'2' && b[i + 1] == b'0') && b[i + 2].is_ascii_digit() && b[i + 3].is_ascii_digit();
        let glued = b[i - 1].is_ascii_alphabetic() && b[i - 2].is_ascii_alphabetic();
        let ends = b.get(i + 4).map_or(true, |c| !c.is_ascii_alphanumeric());
        if year && glued && ends {
            cuts.push(i);
        }
    }
    if cuts.is_empty() {
        return std::borrow::Cow::Borrowed(input);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = String::with_capacity(input.len() + cuts.len());
    let mut last = 0;
    for c in cuts {
        out.push_str(&input[last..c]);
        out.push(' ');
        last = c;
    }
    out.push_str(&input[last..]);
    std::borrow::Cow::Owned(out)
}

fn fold(input: &str) -> Folded {
    let mut f = Folded::default();
    fold_into(input, &mut f);
    f
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Folded> = std::cell::RefCell::new(Folded::default());
}

/// Run `f` on the fold of `input`, reusing a per-thread buffer (matching runs once per paper).
fn with_fold<R>(input: &str, f: impl FnOnce(&Folded) -> R) -> R {
    SCRATCH.with(|s| match s.try_borrow_mut() {
        Ok(mut s) => {
            fold_into(input, &mut s);
            f(&s)
        }
        Err(_) => f(&fold(input)),
    })
}

/// `fold` into a reusable buffer.
fn fold_into(input: &str, f: &mut Folded) {
    let input = &*split_year_suffix(input);
    f.buf.clear();
    f.toks.clear();
    f.buf.reserve(input.len());
    f.toks.reserve(input.len() / 5 + 2);
    let mut cur = std::mem::take(&mut f.cur);
    cur.clear();
    let (mut letters, mut upper, mut first_upper, mut digits_only, mut nchars) =
        (0u32, 0u32, false, true, 0u32);
    let mut prev_apos = false;

    let flush = |cur: &mut String,
                     term_apos: bool,
                     letters: &mut u32,
                     upper: &mut u32,
                     first_upper: &mut bool,
                     digits_only: &mut bool,
                     nchars: &mut u32,
                     prev_apos: &mut bool,
                     f: &mut Folded| {
        if cur.is_empty() {
            *prev_apos = term_apos;
            return;
        }
        let drop = (term_apos && *nchars == 1) || (*prev_apos && cur == "s") || is_stop(cur);
        if !drop {
            let c = canon(cur);
            let s = f.buf.len() as u32;
            f.buf.push_str(c);
            let mut flags = 0;
            if *letters >= 2 && *upper == *letters {
                flags |= F_UPPER;
            }
            if *first_upper {
                flags |= F_CAP;
            }
            if *digits_only {
                flags |= F_NUM;
                if cur.len() == 4 && (cur.starts_with("19") || cur.starts_with("20")) {
                    flags |= F_YEAR;
                }
            }
            f.toks.push(Tok { s, e: f.buf.len() as u32, flags });
        }
        cur.clear();
        *letters = 0;
        *upper = 0;
        *first_upper = false;
        *digits_only = true;
        *nchars = 0;
        *prev_apos = term_apos;
    };

    for c in input.chars().chain(std::iter::once(' ')) {
        if ('\u{300}'..='\u{36f}').contains(&c) {
            continue;
        }
        if c.is_alphanumeric() {
            if cur.is_empty() {
                first_upper = c.is_uppercase();
            }
            if c.is_alphabetic() {
                letters += 1;
                if c.is_uppercase() {
                    upper += 1;
                }
            }
            if !c.is_ascii_digit() {
                digits_only = false;
            }
            nchars += 1;
            push_folded(c, &mut cur);
        } else {
            flush(
                &mut cur,
                is_apostrophe(c),
                &mut letters,
                &mut upper,
                &mut first_upper,
                &mut digits_only,
                &mut nchars,
                &mut prev_apos,
                f,
            );
            if matches!(c, ',' | ';' | '(') {
                if let Some(t) = f.toks.last_mut() {
                    t.flags |= F_BREAK;
                }
            }
        }
    }
    f.cur = cur;
}

// ---------------------------------------------------------------------------------------------
// Entries and index
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Alias {
    /// Written with a leading `^`: the restriction can never be relaxed by a duplicate alias.
    caret: bool,
    toks: &'static [&'static str],
    chars: u32,
    /// Alias is an ALLCAPS acronym: matching text token must be ALLCAPS too.
    upper: bool,
    /// Ambiguous phrase: needs capitalised first token and a following number (unless in cue context).
    vol: bool,
}

/// Token and alias tables are leaked: the base table lives for the whole process and an overlay
/// (`--qfile`) is loaded a handful of times at start-up.
struct Entry {
    venue: Venue,
    name_toks: &'static [&'static str],
    aliases: &'static [Alias],
}

struct Index {
    entries: Vec<Entry>,
    /// (first token, alias chars, entry, alias) sorted by token, then longest alias first.
    first: Vec<(&'static str, u32, u32, u32)>,
    /// Open-addressing table over `first`: (start, len) of the run for a token, len 0 = empty slot.
    slots: Vec<(u32, u32)>,
}

fn hash(t: &str) -> usize {
    t.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193)) as usize
}

/// Result of `parse_alias`; the alias tokens stay in the scratch `Folded`.
struct AliasInfo {
    caret: bool,
    chars: u32,
    upper: bool,
    vol: bool,
}

fn parse_alias(raw: &str, f: &mut Folded) -> Option<AliasInfo> {
    let raw = raw.trim();
    let free = raw.starts_with('!');
    let caret = raw.starts_with('^');
    let raw = raw.trim_start_matches(['!', '^']).trim();
    fold_into(raw, f);
    alias_info(raw, free, caret, f)
}

/// Classify the folded alias `raw` (`f` = its fold).
fn alias_info(raw: &str, free: bool, caret: bool, f: &Folded) -> Option<AliasInfo> {
    let n = f.len();
    if n == 0 {
        return None;
    }
    let chars = ((0..n).map(|i| f.text(i).len()).sum::<usize>() + n - 1) as u32;
    if n >= 2 {
        return Some(AliasInfo { chars, upper: false, vol: caret, caret });
    }
    let letters = raw.chars().filter(|c| c.is_alphabetic()).count();
    let raw_len = raw.chars().filter(|c| c.is_alphanumeric()).count();
    if f.flags(0) & F_UPPER != 0 && letters >= 3 {
        Some(AliasInfo { chars, upper: true, vol: false, caret })
    } else if f.flags(0) & F_UPPER != 0 && letters == 2 {
        // two-letter acronyms (CP, KR, EC) are too ambiguous without a following volume/year
        Some(AliasInfo { chars, upper: true, vol: true, caret })
    } else if raw_len >= 5 || (free && raw_len >= 3) {
        Some(AliasInfo { chars, upper: false, vol: !free, caret })
    } else {
        None
    }
}

fn folded_tokens(f: &Folded) -> impl Iterator<Item = &str> + Clone {
    (0..f.len()).map(move |i| f.text(i))
}

/// Entries under construction: every folded token is stored once, back to back in `text`, and
/// referenced by span; `finish` turns the spans into `'static` slices with a handful of allocations.
#[derive(Default)]
struct Build {
    text: String,
    spans: Vec<Span>,
    aliases: Vec<(AliasInfo, Span)>,
    /// venue, name tokens (into `spans`), aliases (into `aliases`)
    entries: Vec<(Venue, Span, Span)>,
}

/// Half-open index range.
type Span = (u32, u32);

impl Build {
    fn tok(&self, i: u32) -> &str {
        let (s, e) = self.spans[i as usize];
        &self.text[s as usize..e as usize]
    }

    fn push_toks<'a>(&mut self, toks: impl Iterator<Item = &'a str>) -> Span {
        let start = self.spans.len() as u32;
        for t in toks {
            let s = self.text.len() as u32;
            self.text.push_str(t);
            self.spans.push((s, self.text.len() as u32));
        }
        (start, self.spans.len() as u32)
    }

    /// Add an alias to the entry whose aliases start at `first`; an alias with the same tokens is merged.
    fn add_alias<'a>(&mut self, first: usize, toks: impl Iterator<Item = &'a str> + Clone, a: AliasInfo) {
        let dup = (first..self.aliases.len()).find(|&j| {
            let (s, e) = self.aliases[j].1;
            (s..e).map(|k| self.tok(k)).eq(toks.clone())
        });
        match dup {
            Some(j) => {
                let x = &mut self.aliases[j].0;
                if x.caret || a.caret {
                    x.caret = true;
                    x.vol = true;
                } else {
                    x.upper &= a.upper;
                    x.vol &= a.vol;
                }
            }
            None => {
                let span = self.push_toks(toks);
                self.aliases.push((a, span));
            }
        }
    }

    fn add_entry(&mut self, name: &'static str, q: u8, area: &'static str, aliases: &str, inherited: &[Alias], f: &mut Folded) {
        let clean = name.trim_start_matches(['!', '^']);
        fold_into(clean, f);
        let name_span = self.push_toks(folded_tokens(f));
        let first = self.aliases.len();
        // the name itself is the first alias; it folds like `clean` unless whitespace edges differ
        let raw = name.trim();
        let free = raw.starts_with('!');
        let caret = raw.starts_with('^');
        let raw = raw.trim_start_matches(['!', '^']).trim();
        if raw.len() == clean.len() {
            if let Some(a) = alias_info(raw, free, caret, f) {
                self.aliases.push((a, name_span));
            }
        } else if let Some(a) = parse_alias(name, f) {
            self.add_alias(first, folded_tokens(f), a);
        }
        for raw in aliases.split(';') {
            if let Some(a) = parse_alias(raw, f) {
                self.add_alias(first, folded_tokens(f), a);
            }
        }
        for a in inherited {
            let info = AliasInfo { caret: a.caret, chars: a.chars, upper: a.upper, vol: a.vol };
            self.add_alias(first, a.toks.iter().copied(), info);
        }
        self.entries.push((Venue { name: clean, q, area }, name_span, (first as u32, self.aliases.len() as u32)));
    }

    fn finish(self) -> Vec<Entry> {
        let text: &'static str = self.text.leak();
        let pool: &'static [&'static str] = self.spans.iter().map(|&(s, e)| &text[s as usize..e as usize]).collect::<Vec<_>>().leak();
        let slice = |(s, e): (u32, u32)| &pool[s as usize..e as usize];
        let aliases: &'static [Alias] = self
            .aliases
            .iter()
            .map(|(a, t)| Alias { caret: a.caret, toks: slice(*t), chars: a.chars, upper: a.upper, vol: a.vol })
            .collect::<Vec<_>>()
            .leak();
        self.entries
            .iter()
            .map(|&(venue, name, al)| Entry { venue, name_toks: slice(name), aliases: &aliases[al.0 as usize..al.1 as usize] })
            .collect()
    }
}

fn make_entry(name: &'static str, q: u8, area: &'static str, aliases: &str, inherited: &[Alias]) -> Entry {
    let mut b = Build::default();
    b.add_entry(name, q, area, aliases, inherited, &mut Folded::default());
    b.finish().pop().expect("one entry")
}

impl Index {
    fn build(entries: Vec<Entry>) -> Index {
        let mut first = Vec::with_capacity(entries.iter().map(|e| e.aliases.len()).sum());
        for (ei, e) in entries.iter().enumerate() {
            for (ai, a) in e.aliases.iter().enumerate() {
                first.push((a.toks[0], a.chars, ei as u32, ai as u32));
            }
        }
        first.sort_unstable_by_key(|&(t, chars, ei, ai)| (t, std::cmp::Reverse(chars), ei, ai));
        let mut slots = vec![(0, 0); (first.len() * 2).next_power_of_two()];
        let mask = slots.len() - 1;
        let mut start = 0;
        while start < first.len() {
            let t = first[start].0;
            let len = first[start..].partition_point(|x| x.0 == t);
            let mut slot = hash(t) & mask;
            while slots[slot].1 != 0 {
                slot = (slot + 1) & mask;
            }
            slots[slot] = (start as u32, len as u32);
            start += len;
        }
        Index { entries, first, slots }
    }

    /// Aliases starting with token `t`, longest first.
    fn starting_with(&self, t: &str) -> &[(&'static str, u32, u32, u32)] {
        let mask = self.slots.len() - 1;
        let mut slot = hash(t) & mask;
        loop {
            let (start, len) = self.slots[slot];
            if len == 0 {
                return &[];
            }
            let run = &self.first[start as usize..(start + len) as usize];
            if run[0].0 == t {
                return run;
            }
            slot = (slot + 1) & mask;
        }
    }
}

struct Raw<'a> {
    name: &'a str,
    q: u8,
    area: Option<&'a str>,
    aliases: &'a str,
}

fn parse_tsv(tsv: &str) -> Result<Vec<Raw<'_>>, String> {
    parse_lines(tsv, true)
}

/// `check_names`: reject names that fold to nothing (the embedded table is verified by a test instead).
fn parse_lines(tsv: &str, check_names: bool) -> Result<Vec<Raw<'_>>, String> {
    let mut out = Vec::with_capacity(tsv.len() / 64);
    let mut f = Folded::default();
    for (ln, line) in tsv.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split('\t');
        let (Some(name), Some(qcol)) = (cols.next(), cols.next()) else {
            return Err(format!("line {}: expected name<TAB>q[<TAB>area][<TAB>aliases]", ln + 1));
        };
        let (c3, c4) = (cols.next(), cols.next());
        if cols.next().is_some() {
            return Err(format!("line {}: expected name<TAB>q[<TAB>area][<TAB>aliases]", ln + 1));
        }
        let name = name.trim();
        if check_names {
            fold_into(name, &mut f);
            if f.len() == 0 {
                return Err(format!("line {}: empty name", ln + 1));
            }
        }
        let q: u8 = match qcol.trim().trim_start_matches(['q', 'Q']).parse() {
            Ok(q @ 1..=4) => q,
            _ => return Err(format!("line {}: q must be 1..4, got {:?}", ln + 1, qcol)),
        };
        let (area, aliases) = match (c3, c4) {
            (None, _) => (None, ""),
            (Some(a), None) => (None, a),
            (Some(a), Some(al)) => (Some(a.trim()).filter(|a| !a.is_empty()), al),
        };
        out.push(Raw { name, q, area, aliases });
    }
    Ok(out)
}

fn base_raws() -> &'static [Raw<'static>] {
    static RAWS: OnceLock<Vec<Raw<'static>>> = OnceLock::new();
    RAWS.get_or_init(|| parse_lines(DATA, false).unwrap_or_default())
}

fn base() -> &'static Index {
    static BASE: OnceLock<Index> = OnceLock::new();
    BASE.get_or_init(|| {
        let raws = base_raws();
        let n = raws.len();
        let mut b = Build {
            text: String::with_capacity(DATA.len() / 2),
            spans: Vec::with_capacity(n * 6),
            aliases: Vec::with_capacity(n * 3),
            entries: Vec::with_capacity(n),
        };
        let mut f = Folded::default();
        for r in raws {
            b.add_entry(r.name, r.q, r.area.unwrap_or("other"), r.aliases, &[], &mut f);
        }
        Index::build(b.finish())
    })
}

static EXTRA: RwLock<Option<Index>> = RwLock::new(None);

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

// ---------------------------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Mode {
    /// Cue context ("published in X"): ambiguous phrases need no trailing number.
    relax_vol: bool,
    /// Alias must be followed by a 4-digit year ("ICML 2024").
    need_year: bool,
    /// Reject matches preceded by "workshop" (workshop at ICML).
    pre_workshop: bool,
}

#[derive(Clone, Copy)]
struct Cand {
    chars: u32,
    pos: usize,
    venue: Venue,
}

fn better(a: &Cand, b: &Cand) -> bool {
    a.chars > b.chars || (a.chars == b.chars && a.pos < b.pos)
}

fn is_workshop(t: &str) -> bool {
    t == "workshop" || t == "workshops"
}

impl Index {
    fn best(&self, f: &Folded, from: usize, to: usize, mode: Mode) -> Option<Cand> {
        if self.entries.is_empty() {
            return None;
        }
        let mut best: Option<Cand> = None;
        for i in from..to {
            for &(_, _, ei, ai) in self.starting_with(f.text(i)) {
                let e = &self.entries[ei as usize];
                let a = &e.aliases[ai as usize];
                let n = a.toks.len();
                let end = i + n;
                if end > to || a.toks.iter().enumerate().any(|(k, t)| f.text(i + k) != *t) {
                    continue;
                }
                if a.upper && f.flags(i) & F_UPPER == 0 {
                    continue;
                }
                if a.vol
                    && (f.flags(i) & F_CAP == 0
                        || (!mode.relax_vol && (end >= f.len() || f.flags(end) & F_NUM == 0)))
                {
                    continue;
                }
                if mode.need_year && (end >= f.len() || f.flags(end) & F_YEAR == 0) {
                    continue;
                }
                // (conference candidates are rejected after a workshop context in journal references too)
                if (mode.pre_workshop || e.venue.area == "conf") && (from.max(i.saturating_sub(12))..i).any(|k| is_workshop(f.text(k))) {
                    continue;
                }
                // a journal reference starts with its title: "Chinese Ann. Math. Ser. B" is not "Annals of Mathematics"
                // (journal refs: whole text; cue windows: the alias must start the window, i.e. right after the cue)
                let guarded = !mode.pre_workshop || mode.relax_vol;
                if guarded && e.venue.area != "conf" && i > from && f.flags(i - 1) & F_NUM == 0 && {
                    // any preceding word (any case) that is not a publication cue/filler makes this an embedded name
                    let p = f.text(i - 1);
                    // (a lowercase prefix only counts in journal references: comment windows start right after
                    // free text like "accepted for publication in")
                    (f.flags(i - 1) & F_CAP != 0 || (!mode.pre_workshop && p.chars().count() >= 4))
                        && !matches!(p, "published" | "appeared" | "appears" | "appear" | "accepted" | "forthcoming" | "submitted" | "see" | "ref" | "reference" | "also" | "previously" | "presented")
                } {
                    continue;
                }
                // a journal title that continues with another capitalised word ("Acta Mathematica Vietnamica") is a
                // different journal; series/part markers and single letters are fine
                // (full titles only: an acronym like A&A is legitimately followed by "Letter")
                if e.venue.area != "conf" && a.chars >= 8 {
                    // a comma/semicolon right after the title ("..., revised version") ends it
                    let broken = f.flags(end - 1) & F_BREAK != 0;
                    if let Some(next) = (end < f.len() && !broken).then(|| f.text(end)) {
                        // (a following all-caps token is the journal's own acronym: "Journal of ... Research (JMLR) 24")
                        // (any case: "Acta mathematica vietnamica" continues the title just like "Vietnamica")
                        let cont = (f.flags(end) & F_UPPER == 0 || next.chars().count() > 6) && f.flags(end) & F_NUM == 0 && next.chars().count() >= 3 // (canonical tokens are shortened: Sinica -> sin)
                            && !matches!(next, "ser" | "sup" | "suppl" | "appear" | "appears" | "press" | "forthcoming" | "published" | "accepted" | "submitted" | "preprint" | "arxiv" | "series" | "second" | "part" | "new" | "supplement" | "rapid" | "regular" | "brief" | "erratum" | "comment" | "reply" | "volume" | "vol" | "issue" | "number" | "no" | "pages" | "page" | "article");
                        if cont {
                            continue;
                        }
                    }
                }
                // skip a trailing year / "(ICML 2024)" style acronym before checking for "Workshop"
                let mut k = end;
                while k < f.len() && k < end + 4 && f.flags(k) & (F_NUM | F_UPPER) != 0 {
                    k += 1;
                }
                // "Machine Learning for Molecules Workshop": a workshop title that merely contains a venue name
                let mut w = k;
                let mut is_ws = false;
                // (a few tokens only, and not across a separate remark like "previously presented at ...")
                while w < f.len() && w < end + 4 && f.flags(w) & F_YEAR == 0 {
                    if is_workshop(f.text(w)) {
                        is_ws = true;
                        break;
                    }
                    if matches!(f.text(w), "previously" | "presented" | "also" | "extended" | "earlier" | "version") {
                        break;
                    }
                    w += 1;
                }
                if is_ws || (k < f.len() && is_workshop(f.text(k))) {
                    continue;
                }
                // "Proceedings of the IEEE International Conference on ...": a conference title, not that journal
                if e.venue.area != "conf" {
                    let mut conf_title = false;
                    for k in end..(end + 6).min(f.len()) {
                        if f.flags(k) & F_NUM != 0 {
                            break; // volume/year: the bibliographic tail starts
                        }
                        if matches!(f.text(k), "conference" | "symposium" | "workshop" | "conf") {
                            conf_title = true;
                            break;
                        }
                    }
                    if conf_title {
                        continue;
                    }
                }
                let c = Cand { chars: a.chars, pos: i, venue: e.venue };
                if best.as_ref().map_or(true, |b| better(&c, b)) {
                    best = Some(c);
                }
                break;
            }
        }
        best
    }
}

/// Best candidate over overlay (wins ties) and base index.
fn find(f: &Folded, from: usize, to: usize, mode: Mode) -> Option<Cand> {
    let g = EXTRA.read().unwrap_or_else(|e| e.into_inner());
    let o = g.as_ref().and_then(|x| x.best(f, from, to, mode));
    let b = base().best(f, from, to, mode);
    match (o, b) {
        (Some(o), Some(b)) => Some(if better(&b, &o) { b } else { o }),
        (o, b) => o.or(b),
    }
}

fn tok_close(a: &str, b: &str) -> bool {
    a == b || (a.len() >= 7 && b.len() >= 7 && edit1(a, b))
}

fn edit1(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (mut i, mut j, mut d) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else {
            d += 1;
            if d > 1 {
                return false;
            }
            if a.len() > b.len() {
                i += 1;
            } else if b.len() > a.len() {
                j += 1;
            } else {
                i += 1;
                j += 1;
            }
        }
    }
    d + (a.len() - i) + (b.len() - j) <= 1
}

/// Visit every live entry (overlay first; base entries shadowed by an overlay entry of the same name are skipped).
fn visit(mut f: impl FnMut(&Entry)) {
    let g = EXTRA.read().unwrap_or_else(|e| e.into_inner());
    if let Some(x) = g.as_ref() {
        x.entries.iter().for_each(&mut f);
    }
    for e in &base().entries {
        let shadowed = g.as_ref().is_some_and(|x| x.entries.iter().any(|o| o.name_toks == e.name_toks));
        if !shadowed {
            f(e);
        }
    }
}

fn fuzzy(f: &Folded) -> Option<Venue> {
    let mut q: Vec<&str> = Vec::new();
    let mut all_upper = true;
    for i in 0..f.len() {
        let t = f.text(i);
        if f.flags(i) & F_NUM != 0 || matches!(t, "vol" | "no" | "pp" | "p" | "pages") || q.contains(&t) {
            continue;
        }
        all_upper &= f.flags(i) & F_UPPER != 0;
        q.push(t);
    }
    if q.is_empty() || q.len() > 12 {
        return None;
    }
    let mut best: Option<(f32, Venue)> = None;
    visit(|e| {
        let v = e.name_toks;
        if v.len() > 64 || 10 * q.len().min(v.len()) < 9 * q.len().max(v.len()) {
            return;
        }
        if !all_upper && e.aliases.iter().any(|a| a.upper && a.toks == v) {
            return;
        }
        let mut used = 0u64;
        let mut m = 0;
        for qt in &q {
            if let Some(j) = (0..v.len()).find(|&j| used & (1 << j) == 0 && tok_close(qt, v[j])) {
                used |= 1 << j;
                m += 1;
            }
        }
        let dice = 2.0 * m as f32 / (q.len() + v.len()) as f32;
        if dice >= 0.9 && best.as_ref().map_or(true, |b| dice > b.0) {
            best = Some((dice, e.venue));
        }
    });
    best.map(|b| b.1)
}

/// Best match for a free-text journal_ref ("Ann. of Math. 198 (2023) 1-50") or plain journal name.
pub fn resolve(jref: &str) -> Option<Venue> {
    with_fold(jref, |f| resolve_folded(jref, f))
}

fn resolve_folded(jref: &str, f: &Folded) -> Option<Venue> {
    if f.len() == 0 {
        return None;
    }
    let mode = Mode { relax_vol: false, need_year: false, pre_workshop: false };
    if let Some(c) = find(f, 0, f.len(), mode) {
        return Some(c.venue);
    }
    if jref.len() < 80 {
        fuzzy(f)
    } else {
        None
    }
}

/// Token `i` starts a "not published yet" clause: submitted / submission / under review / in review.
fn pending_at(f: &Folded, i: usize) -> bool {
    match f.text(i) {
        "submitted" | "submission" | "rejected" | "withdrawn" => true,
        // "in review at X" loses its "in" while folding, so a clause-initial "review" counts too
        "review" | "rev" | "reviewed" => i == 0 || matches!(f.text(i - 1), "under" | "in" | "being"),
        _ => false,
    }
}

/// Raw-text check for "not published yet" clauses, run before stop-word folding eats the context
/// ("in review at ICML 2024" folds to "review icml 2024").
fn raw_pending(c: &str) -> bool {
    let l = c.to_lowercase();
    ["under review", "being reviewed", "submitted", "submission", "under consideration"].iter().any(|p| l.contains(p))
        || l.match_indices("in review").any(|(i, m)| {
            let rest = &l[i + m.len()..];
            rest.is_empty() || rest.starts_with([',', '.', ';', ')', ' ']) && {
                let w = rest.trim_start_matches([',', '.', ';', ')', ' ']);
                w.is_empty() || ["at", "for", "by", "to", "with", "and", "in"].iter().any(|p| w.starts_with(p) && w[p.len()..].starts_with([' ', ',', '.']))
            }
        })
}

/// A publication cue at `i` is negated ("not accepted", "not to appear", "not yet published").
fn negated_at(f: &Folded, i: usize) -> bool {
    negated_from(f, i, false)
}

/// `inherit`: the token at `i` is not itself a publication verb (e.g. "Proceedings"), so it shares the
/// negation of the verb that governs it.
fn negated_from(f: &Folded, i: usize, inherit: bool) -> bool {
    // look back a few tokens, but stop at an earlier publication cue: "not accepted to X, accepted to Y"
    for k in (i.saturating_sub(5)..i).rev() {
        if f.flags(k) & F_BREAK != 0 {
            return false; // a comma/semicolon starts a new remark: "not the final version, accepted for ..."
        }
        match f.text(k) {
            "not" | "never" | "yet" | "un" | "hasn" | "isn" | "wasn" | "haven" | "weren" | "won" => return true,
            "accepted" | "appear" | "appears" | "appeared" | "published" | "forthcoming" | "press" => return inherit && negated_from(f, k, false),
            _ => {}
        }
    }
    false
}

fn is_noise_unit(t: &str) -> bool {
    matches!(t, "pages" | "page" | "pp" | "pgs" | "figures" | "figure" | "figs" | "fig" | "tables" | "table")
}

/// Like `resolve` for arXiv comments: only fires after cue words (accepted/to appear/published in/proceedings of...).
pub fn resolve_comment(comment: &str) -> Option<Venue> {
    // negation / "submitted" only apply inside their own clause: "No figures. Accepted to ICML 2024"
    clauses(comment).into_iter().flat_map(split_but).find_map(resolve_clause)
}

/// Contrast starts a new clause: "Not the final version, but accepted to ICML 2024".
fn split_but(c: &str) -> Vec<&str> {
    let lower = c.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut start = 0;
    for (i, _) in lower.match_indices(" but ") {
        out.push(&c[start..i]);
        start = i + 5;
    }
    out.push(&c[start..]);
    out
}

const SENTENCE_STARTS: &[&str] = &[
    "accepted", "submitted", "not", "under", "published", "this", "work", "preprint", "extended", "see", "comments",
    "revised", "appeared", "appears", "camera", "final", "earlier", "previous", "version", "also", "note",
];

/// Split at ';', newlines and sentence ends (". " + capital after a word of 5+ letters or a digit, so
/// abbreviations like "Ann. Math." stay together).
fn clauses(c: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, ch) in c.char_indices() {
        let split = match ch {
            ';' | '\n' => true,
            // a sentence end, not a journal abbreviation ("Trans. Amer. Math. Soc."): the next word must
            // look like the start of a new remark
            '.' => c[i + 1..].strip_prefix(' ').map_or(false, |r| {
                let w: String = r.chars().take_while(|x| x.is_alphabetic()).collect::<String>().to_lowercase();
                SENTENCE_STARTS.contains(&w.as_str())
            }),
            _ => false,
        };
        if split {
            out.push(&c[start..i]);
            start = i + 1;
        }
    }
    out.push(&c[start..]);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

fn resolve_clause(comment: &str) -> Option<Venue> {
    with_fold(comment, |f| clause_venue(comment, f))
}

fn clause_venue(comment: &str, f: &Folded) -> Option<Venue> {
    let n = f.len();
    if n == 0 {
        return None;
    }
    let mut best: Option<Cand> = None;
    let cue_mode = Mode { relax_vol: true, need_year: false, pre_workshop: true };
    for i in 0..n {
        let start = match f.text(i) {
            "accepted" | "appear" | "appears" | "appeared" | "published" | "forthcoming" | "press" if negated_at(f, i) => continue,
            "accepted" | "appear" | "appears" | "appeared" | "published" | "forthcoming" | "press" => i + 1,
            "camera" if i + 1 < n && f.text(i + 1) == "ready" => i + 2,
            "proc" if !(0..i).any(|k| pending_at(f, k)) && !negated_from(f, i, true) => i,
            _ => continue,
        };
        let mut end = (start + 24).min(n);
        // an acceptance clause ends where a later "submitted/under review" clause begins
        if let Some(j) = (start..end).find(|&j| pending_at(f, j)) {
            end = j;
        }
        if let Some(j) = (start..end.saturating_sub(1))
            .find(|&j| f.flags(j) & F_NUM != 0 && is_noise_unit(f.text(j + 1)))
        {
            end = j;
        }
        let forward = if start < end { nearest(f, start, end, cue_mode) } else { None };
        if forward.is_some() {
            consider(&mut best, forward);
        } else {
            // postfix form: "Annals of Combinatorics, to appear" / "Journal X, In press"
            consider(&mut best, find(f, i.saturating_sub(12), i, cue_mode));
        }
    }
    // "Submitted to ICML 2024" / "under review at X" is not a publication
    let pending = raw_pending(comment) || (0..n).any(|i| {
        pending_at(f, i) || (matches!(f.text(i), "accepted" | "appear" | "appears" | "appeared" | "published" | "forthcoming" | "press") && negated_at(f, i))
    });
    if !pending && best.is_none() {
        consider(&mut best, find(f, 0, n, Mode { relax_vol: false, need_year: true, pre_workshop: true }));
    }
    best.map(|c| c.venue)
}

fn consider(best: &mut Option<Cand>, c: Option<Cand>) {
    if let Some(c) = c {
        if best.as_ref().map_or(true, |b| better(&c, b)) {
            *best = Some(c);
        }
    }
}

/// Venue closest to the cue: shrink the window to end before each candidate until none remains, so
/// "Accepted to ALT 2024, extension of an ICML 2023 paper" picks ALT (the governed venue), not the
/// longer alias that merely appears later.
fn nearest(f: &Folded, start: usize, end: usize, mode: Mode) -> Option<Cand> {
    let mut best = find(f, start, end, mode)?;
    while let Some(earlier) = find(f, start, best.pos, mode) {
        best = earlier;
    }
    Some(best)
}

fn score(q: &[&str], v: &[&str]) -> f32 {
    if q.is_empty() || v.is_empty() {
        return 0.0;
    }
    if q == v {
        return 1.0;
    }
    if q.len() <= v.len() {
        if let Some(s) = (0..=v.len() - q.len()).find(|&s| v[s..s + q.len()] == *q) {
            let _ = s;
            let qc: usize = q.iter().map(|t| t.len()).sum();
            let vc: usize = v.iter().map(|t| t.len()).sum();
            return 0.7 + 0.29 * qc as f32 / vc as f32;
        }
    }
    let mut stack = [false; 16];
    let mut heap;
    let used: &mut [bool] = if v.len() <= stack.len() {
        &mut stack[..v.len()]
    } else {
        heap = vec![false; v.len()];
        &mut heap
    };
    let mut m = 0;
    for qt in q {
        let hit = (0..v.len()).find(|&j| !used[j] && (*qt == v[j] || (qt.len() >= 3 && v[j].starts_with(qt)) || tok_close(qt, v[j])));
        if let Some(j) = hit {
            used[j] = true;
            m += 1;
        }
    }
    0.8 * 2.0 * m as f32 / (q.len() + v.len()) as f32
}

/// Fuzzy name search for the `venue` command; best first.
pub fn lookup(name: &str, limit: usize) -> Vec<(Venue, f32)> {
    let f = fold(name);
    let q: Vec<&str> = folded_tokens(&f).collect();
    if q.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut hits: Vec<(Venue, f32)> = Vec::new();
    visit(|e| {
        let s = e
            .aliases
            .iter()
            .map(|a| score(&q, a.toks))
            .fold(score(&q, e.name_toks), f32::max);
        if s >= 0.5 {
            hits.push((e.venue, s));
        }
    });
    hits.sort_by(|a, b| {
        b.1.total_cmp(&a.1).then(a.0.q.cmp(&b.0.q)).then_with(|| a.0.name.cmp(b.0.name))
    });
    hits.truncate(limit);
    hits
}

/// Extend table from user TSV text: `name<TAB>q<TAB>alias;alias;...` (q = 1..4). Err on bad line.
/// A 4-column line is `name<TAB>q<TAB>area<TAB>aliases`. A name equal to a built-in venue overrides it
/// (and keeps its aliases). Nothing is loaded when any line is bad.
pub fn load_extra(tsv: &str) -> Result<usize, String> {
    let raws = parse_tsv(tsv)?;
    let count = raws.len();
    let b = base();
    let mut g = EXTRA.write().unwrap_or_else(|e| e.into_inner());
    let mut entries: Vec<Entry> = g.take().map(|x| x.entries).unwrap_or_default();
    let mut f = Folded::default();
    for r in raws {
        fold_into(r.name.trim_start_matches(['!', '^']), &mut f);
        let key: Vec<&str> = folded_tokens(&f).collect();
        let old = b.entries.iter().find(|e| e.name_toks == key);
        let area = r.area.map(leak).or(old.map(|e| e.venue.area)).unwrap_or("extra");
        let inherited = old.map_or(&[][..], |e| e.aliases);
        entries.retain(|e| e.name_toks != key);
        entries.push(make_entry(leak(r.name), r.q, area, r.aliases, inherited));
    }
    *g = Some(Index::build(entries));
    Ok(count)
}

/// Compact listing for `arx venue` with no args / `arx venue --q 1,2 --cat math`.
/// `q_mask`: bit (q-1) set for each quartile to show (0b1111 = all).
pub fn list(q_mask: u8, area: Option<&str>) -> String {
    let area = area.map(str::to_lowercase);
    let mut rows: Vec<Venue> = Vec::new();
    let mut take = |v: Venue| {
        if q_mask & (1 << (v.q - 1)) != 0 && area.as_deref().is_none_or(|a| crate::taxonomy::contains_lower(v.area, a)) {
            rows.push(v);
        }
    };
    // without an overlay the table is only read, never folded
    if EXTRA.read().unwrap_or_else(|e| e.into_inner()).is_none() {
        for r in base_raws() {
            take(Venue { name: r.name.trim_start_matches(['!', '^']), q: r.q, area: r.area.unwrap_or("other") });
        }
    } else {
        visit(|e| take(e.venue));
    }
    rows.sort_by_cached_key(|v| (v.q, v.name.to_lowercase()));
    let mut out = String::with_capacity(rows.len() * 40);
    for (i, v) in rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = write!(out, "Q{} {} [{}]", v.q, v.name, v.area);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Option<(&'static str, u8)> {
        resolve(s).map(|v| (v.name, v.q))
    }
    fn rc(s: &str) -> Option<(&'static str, u8)> {
        resolve_comment(s).map(|v| (v.name, v.q))
    }

    #[test]
    fn annals() {
        assert_eq!(r("Ann. of Math. 198 (2023), no. 3, 1-50"), Some(("Annals of Mathematics", 1)));
        assert_eq!(r("The Annals of Mathematics"), Some(("Annals of Mathematics", 1)));
    }

    #[test]
    fn jcta_variants() {
        let want = Some(("Journal of Combinatorial Theory, Series A", 1));
        assert_eq!(r("J. Comb. Theory Ser. A 150 (2017) 1-30"), want);
        assert_eq!(r("J. Combin. Theory Ser. A, 150 (2017), 1-30."), want);
        assert_eq!(r("Journal of Combinatorial Theory, Series A 150"), want);
        assert_eq!(r("J. Combin. Theory Ser. B 99 (2009)"), Some(("Journal of Combinatorial Theory, Series B", 1)));
    }

    #[test]
    fn ams_journals() {
        assert_eq!(
            r("Trans. Amer. Math. Soc. 375 (2022), 4001-4040"),
            Some(("Transactions of the American Mathematical Society", 1))
        );
        assert_eq!(
            r("Proc. Amer. Math. Soc. 150 (2022) 123-130"),
            Some(("Proceedings of the American Mathematical Society", 1))
        );
        assert_eq!(
            r("Proc. Amer. Math. Soc. Ser. B 9 (2022)"),
            Some(("Proceedings of the American Mathematical Society, Series B", 1))
        );
    }

    #[test]
    fn combinatorics_and_education() {
        assert_eq!(r("Electron. J. Combin. 29(2) (2022), #P2.10"), Some(("Electronic Journal of Combinatorics", 2)));
        assert_eq!(r("Amer. Math. Monthly 129 (2022), 123-140"), Some(("The American Mathematical Monthly", 2)));
        assert_eq!(r("Integers 21 (2021), Paper A5"), Some(("Integers", 3)));
        assert_eq!(r("Combin. Probab. Comput. 31 (2022)"), Some(("Combinatorics, Probability and Computing", 1)));
        assert_eq!(r("Forum Geom. 14 (2014) 1-6"), Some(("Forum Geometricorum", 4)));
        assert_eq!(r("Elem. Math. 77 (2022)"), Some(("Elemente der Mathematik", 3)));
        assert_eq!(r("Math. Mag. 95 (2022)"), Some(("Mathematics Magazine", 3)));
        assert_eq!(r("Crux Math. 48"), Some(("Crux Mathematicorum", 4)));
    }

    #[test]
    fn physics_and_diacritics() {
        assert_eq!(r("Phys. Rev. Lett. 130, 010401 (2023)"), Some(("Physical Review Letters", 1)));
        assert_eq!(r("Phys. Rev. D 107, 024001"), Some(("Physical Review D", 1)));
        assert_eq!(r("Annales de l'Institut Fourier 72 (2022)"), Some(("Annales de l'Institut Fourier", 1)));
        assert_eq!(
            r("J. reine angew. Math. 780 (2021)"),
            Some(("Journal für die reine und angewandte Mathematik", 1))
        );
        assert_eq!(
            r("Crelle's Journal 2020"),
            Some(("Journal für die reine und angewandte Mathematik", 1))
        );
        assert_eq!(r("Commun. Math. Phys. 392 (2022)"), Some(("Communications in Mathematical Physics", 1)));
    }

    #[test]
    fn longest_alias_wins_over_prefix() {
        assert_eq!(r("Adv. Math. Phys. 2020"), Some(("Advances in Mathematical Physics", 2)));
        assert_eq!(r("Adv. Math. 400 (2022)"), Some(("Advances in Mathematics", 1)));
        assert_eq!(r("Ann. Math. Stat. 20 (1949)"), Some(("Annals of Mathematical Statistics", 1)));
        assert_eq!(r("Ann. Probab. 50 (2022)"), Some(("Annals of Probability", 1)));
        assert_eq!(r("Discrete Math. Theor. Comput. Sci. 24"), Some(("Discrete Mathematics & Theoretical Computer Science", 3)));
    }

    #[test]
    fn ambiguous_phrases_need_volume() {
        assert_eq!(r("Nature 596, 583 (2021)"), Some(("Nature", 1)));
        assert_eq!(r("Quantum 5, 456 (2021)"), Some(("Quantum", 1)));
        assert_eq!(r("Machine Learning 110 (2021)"), Some(("Machine Learning", 1)));
        assert_eq!(r("Quantum 5"), Some(("Quantum", 1)));
        assert_eq!(rc("Published in the Machine Learning journal of nothing"), Some(("Machine Learning", 1)));
        assert_eq!(rc("We study quantum machine learning and integers of nature"), None);
    }

    #[test]
    fn logic_tcs_stat_ai() {
        assert_eq!(r("Log. Methods Comput. Sci. 18 (2022)"), Some(("Logical Methods in Computer Science", 2)));
        assert_eq!(r("J. Symbolic Logic 87 (2022)"), Some(("Journal of Symbolic Logic", 1)));
        assert_eq!(r("SIAM J. Optim. 32 (2022)"), Some(("SIAM Journal on Optimization", 1)));
        assert_eq!(r("J. ACM 69 (2022)"), Some(("Journal of the ACM", 1)));
        assert_eq!(r("Ann. Statist. 50 (2022)"), Some(("Annals of Statistics", 1)));
        assert_eq!(r("J. Mach. Learn. Res. 22"), Some(("Journal of Machine Learning Research", 1)));
        assert_eq!(r("Math. Comput. Modelling 50"), Some(("Mathematical and Computer Modelling", 2)));
        assert_eq!(r("Ark. Mat. 60 (2022)"), Some(("Arkiv för Matematik", 2)));
    }

    #[test]
    fn fold_normalises_text() {
        let f = fold("Annales de l\u{2019}Institut Fourier, Crelle's J. \u{c9}cole e\u{301}");
        let toks: Vec<&str> = (0..f.len()).map(|i| f.text(i)).collect();
        assert_eq!(toks, ["ann", "inst", "fourier", "crelle", "j", "ecole", "e"]);
    }

    #[test]
    fn tsv_parsing_columns() {
        let raws = parse_tsv("#c\nA B\t2\nC D\tq3\tx;y\nE F\t1\tarea\tz\n").unwrap();
        assert_eq!(raws.len(), 3);
        assert_eq!((raws[0].q, raws[0].area, raws[0].aliases), (2, None, ""));
        assert_eq!((raws[1].q, raws[1].area, raws[1].aliases), (3, None, "x;y"));
        assert_eq!((raws[2].q, raws[2].area, raws[2].aliases), (1, Some("area"), "z"));
    }

    #[test]
    fn typo_tolerance_is_bounded() {
        assert!(edit1("combinatorica", "combinatoria"));
        assert!(!edit1("combinatorica", "combinatoris!"));
        assert!(tok_close("algorithmica", "algorithmika"));
        assert!(!tok_close("math", "mats"));
    }

    #[test]
    fn negatives() {
        assert_eq!(r("Journal of Imaginary Results 12 (2020)"), None);
        assert_eq!(r("ACM"), None);
        assert_eq!(r("Math"), None);
        assert_eq!(r("Math. 2020"), None);
        assert_eq!(r("Proceedings"), None);
        assert_eq!(r(""), None);
        assert_eq!(r("   ...   "), None);
        assert_eq!(r("acl"), None);
        assert_eq!(r("Some random text about integers and nature of things"), None);
    }

    #[test]
    fn fuzzy_plain_names() {
        assert_eq!(r("Fields Related Theory and Probability"), Some(("Probability Theory and Related Fields", 1)));
        assert_eq!(r("Nature"), Some(("Nature", 1)));
        assert_eq!(r("Combinatorica"), Some(("Combinatorica", 1)));
        let long = format!("{} Fields Related Theory and Probability", "word ".repeat(20));
        assert_eq!(r(&long), None);
    }

    #[test]
    fn junk_does_not_panic() {
        assert_eq!(r("\u{0}\u{ffff}日本語 😀 é\u{301} ß ł"), None);
        assert_eq!(rc("accepted \u{1F600}\u{1F600} 12 pages"), None);
        assert_eq!(r("e\u{301}"), None);
        assert!(lookup("😀", 5).is_empty());
    }

    #[test]
    fn comment_cues() {
        assert_eq!(rc("12 pages, 3 figures. Accepted to ICML 2024"), Some(("ICML", 1)));
        assert_eq!(rc("To appear in Proc. Amer. Math. Soc."), Some(("Proceedings of the American Mathematical Society", 1)));
        assert_eq!(
            rc("Accepted for publication in Annals of Probability. 25 pages"),
            Some(("Annals of Probability", 1))
        );
        assert_eq!(rc("Camera-ready version for NeurIPS"), Some(("NeurIPS", 1)));
        assert_eq!(
            rc("Published in Proceedings of the 56th Annual ACM Symposium on Theory of Computing (STOC 2024)"),
            Some(("STOC", 1))
        );
        assert_eq!(rc("Accepted to ACL 2024 Findings"), Some(("ACL", 1)));
    }

    #[test]
    fn comment_year_pattern_without_cue() {
        assert_eq!(rc("NeurIPS 2023 spotlight, 30 pages"), Some(("NeurIPS", 1)));
        assert_eq!(rc("Extended version of the CVPR 2022 paper"), Some(("CVPR", 1)));
    }

    #[test]
    fn comment_negatives() {
        assert_eq!(rc("Submitted to Annals of Mathematics"), None);
        assert_eq!(rc("We prove the Annals of Mathematics result"), None);
        assert_eq!(rc("12 pages, 3 figures"), None);
        assert_eq!(rc("Accepted to a workshop at ICML 2024"), None);
        assert_eq!(rc("Accepted at the ICLR 2024 Workshop on Tiny Papers"), None);
        assert_eq!(rc("Accepted at acl"), None);
        assert_eq!(rc("Accepted to ACM"), None);
        assert_eq!(rc(""), None);
    }

    #[test]
    fn comment_noise_window_cut() {
        assert_eq!(rc("Accepted, 12 pages, 3 figures, Annals of Probability"), None);
    }

    #[test]
    fn lookup_ranks_exact_first() {
        let hits = lookup("annals of math", 3);
        assert_eq!(hits[0].0.name, "Annals of Mathematics");
        assert!(hits[0].1 >= 0.99);
        assert!(hits.len() <= 3);
        assert_eq!(lookup("stoc", 1)[0].0.name, "STOC");
        assert!(lookup("zzzz qqqq", 5).is_empty());
        assert!(lookup("annals", 0).is_empty());
    }

    #[test]
    fn list_filters_and_sorts() {
        let confs = list(0b0001, Some("CONF"));
        assert!(confs.lines().any(|l| l == "Q1 ICML [conf]"));
        assert!(confs.lines().all(|l| l.starts_with("Q1 ") && l.ends_with("[conf]")));
        let names: Vec<&str> = confs.lines().collect();
        let mut sorted = names.clone();
        sorted.sort_by_key(|l| l.to_lowercase());
        assert_eq!(names, sorted);
        let all = list(0b1111, None);
        assert!(all.starts_with("Q1 "));
        let qs: Vec<char> = all.lines().map(|l| l.as_bytes()[1] as char).collect();
        assert!(qs.windows(2).all(|w| w[0] <= w[1]));
        assert!(list(0b1000, Some("edu")).contains("Q4 Crux Mathematicorum [math.edu]"));
        assert_eq!(list(0, None), "");
    }

    #[test]
    fn embedded_data_is_well_formed() {
        let raws = parse_tsv(DATA).expect("embedded TSV parses");
        assert!(raws.len() >= 350, "only {} venues", raws.len());
        assert!(raws.iter().all(|r| r.area.is_some()));
        let b = base();
        assert_eq!(b.entries.len(), raws.len());
        let mut seen: std::collections::HashMap<&[&str], &str> = std::collections::HashMap::new();
        for e in &b.entries {
            assert!(!e.aliases.is_empty(), "{} has no alias", e.venue.name);
            for a in e.aliases {
                if let Some(prev) = seen.insert(a.toks, e.venue.name) {
                    assert_eq!(prev, e.venue.name, "alias {:?} shared by two venues", a.toks);
                }
            }
        }
    }

    #[test]
    fn load_extra_adds_overrides_and_rejects() {
        assert_eq!(r("Kodai Math. J. 12 (1988) 1-10"), Some(("Kodai Mathematical Journal", 3)));
        let n = load_extra(
            "# comment\n\nKodai Mathematical Journal\t1\nJournal of Zebra Mathematics\t4\tzebra\tJ. Zebra Math.;ZMJ\r\n",
        )
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(r("Kodai Math. J. 12 (1988) 1-10"), Some(("Kodai Mathematical Journal", 1)));
        assert_eq!(resolve("Kodai Math. J. 12").unwrap().area, "math");
        assert_eq!(r("J. Zebra Math. 3 (2020)"), Some(("Journal of Zebra Mathematics", 4)));
        assert_eq!(rc("accepted to ZMJ"), Some(("Journal of Zebra Mathematics", 4)));
        assert!(list(0b1000, Some("zebra")).contains("Q4 Journal of Zebra Mathematics [zebra]"));
        assert_eq!(list(0b0100, None).lines().filter(|l| l.contains("Kodai")).count(), 0);
        assert_eq!(lookup("zebra mathematics", 1)[0].0.q, 4);
        assert_eq!(r("Ann. of Math. 198 (2023)"), Some(("Annals of Mathematics", 1)));

        assert!(load_extra("Bad Line No Tab").is_err());
        assert!(load_extra("X\t9").unwrap_err().contains("line 1"));
        assert!(load_extra("Journal of Never Loaded\t2\nY\tabc").is_err());
        assert_eq!(r("Journal of Never Loaded 3"), None);
        assert!(load_extra("A\t1\tb\tc\td").is_err());
        assert!(load_extra("\t2").is_err());
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;

    #[test]
    fn submitted_is_not_published() {
        assert!(resolve_comment("Submitted to ICML 2024").is_none());
        assert_eq!(
            resolve_comment("Accepted to ICML 2024; extended version submitted to Journal of Machine Learning Research").map(|v| v.name),
            Some("ICML")
        );
        assert_eq!(resolve_comment("Accepted to ALT 2024, extension of an ICML 2023 submission").map(|v| v.name), Some("ALT"));
        assert_eq!(resolve_comment("Accepted to ICML 2024, previously presented at a workshop").map(|v| v.name), Some("ICML"));
        assert_eq!(resolve_comment("Not the final version, but accepted to ICML 2024").map(|v| v.name), Some("ICML"));
        assert!(resolve_comment("Rejected from ICML 2024").is_none());
        assert!(resolve_comment("accepted to Machine Learning for Molecules Workshop @ NeurIPS 2020").is_none());
        assert!(resolve_comment("Accepted to the Workshop on Generative Models and Decision Making at ICML 2024").is_none());
        assert!(resolve_comment("Under review at NeurIPS 2024").is_none());
        assert_eq!(resolve_comment("Accepted to Physical Review Letters").map(|v| v.q), Some(1));
        assert!(resolve_comment("Not accepted to ICML 2024").is_none());
        assert!(resolve_comment("In review at NeurIPS 2024").is_none());
        assert!(resolve_comment("12 pages; in review at ICML 2024").is_none());
        assert_eq!(resolve_comment("Accepted in Review of Modern Physics").is_some(), true);
        assert!(resolve_comment("Not published in Nature").is_none());
        assert_eq!(resolve_comment("Accepted to Trans. Amer. Math. Soc.").map(|v| v.q), Some(1));
        assert!(resolve_comment("Accepted to Phys. Rev. A").is_some());
        assert!(resolve_comment("To appear in J. Phys. A").is_some());
        assert_eq!(resolve_comment("The Electronic Journal of Combinatorics, In press").map(|v| v.q), Some(2));
        assert_eq!(resolve_comment("Annals of Combinatorics, to appear").map(|v| v.q), Some(2));
        assert_eq!(resolve_comment("Not accepted to ICML 2023, accepted to ICLR 2024").map(|v| v.name), Some("ICLR"));
        assert_eq!(resolve_comment("No figures. Accepted to ICML 2024").map(|v| v.name), Some("ICML"));
        assert_eq!(resolve_comment("10 pages, no figures, accepted to ICML 2024").map(|v| v.name), Some("ICML"));
        assert!(resolve_comment("This paper hasn't been accepted to ICML 2024").is_none());
        assert!(resolve_comment("Not accepted to Proceedings of the American Mathematical Society").is_none());
        assert_eq!(resolve_comment("In press at Journal of Machine Learning Research").map(|v| v.q), Some(1));
        assert!(resolve_comment("This paper has not been formally accepted to ICML 2024").is_none());
        assert!(resolve_comment("Not to appear in ICML 2024").is_none());
        assert!(resolve_comment("Accepted to International Conference on Machine Learning (ICML 2024) Workshop on X").is_none());
        assert!(resolve_comment("Submitted to Proceedings of the American Mathematical Society").is_none());
        assert_eq!(resolve_comment("Accepted to ICML 2024").map(|v| v.q), Some(1));
    }

    #[test]
    fn caret_restriction_survives_duplicate_alias() {
        assert!(resolve("we study machine learning models").is_none());
        assert!(resolve("Machine Learning 112 (2023) 1-20").is_some());
    }

    #[test]
    fn commas_inside_names_do_not_create_aliases() {
        assert!(resolve_comment("Accepted to International Conference on Logic Programming (ICLP 2024)").map_or(true, |v| v.name != "LPAR"));
        assert!(lookup("Programming", 5).iter().all(|(v, s)| v.name != "ICALP" || *s < 0.99));
    }

    #[test]
    fn conference_references_keep_their_venue() {
        assert_eq!(
            resolve("2024 IEEE/CVF Conference on Computer Vision and Pattern Recognition (CVPR), pp. 1234-1242").map(|v| v.name),
            Some("CVPR")
        );
    }

    #[test]
    fn embedded_journal_names_in_comments() {
        assert!(resolve_comment("31 pages;accepted by Chinese Annals of Mathematics. Series A").is_none_or(|v| v.name != "Annals of Mathematics"));
        assert_eq!(resolve_comment("accepted for publication in Annals of Mathematics").map(|v| v.name), Some("Annals of Mathematics"));
    }

    #[test]
    fn attached_series_volume() {
        assert_eq!(resolve("J.Phys.A39:8933-8964,2006").map(|v| v.name), Some("Journal of Physics A: Mathematical and Theoretical"));
    }

    #[test]
    fn lowercase_embedded_prefix_is_rejected() {
        assert!(resolve("chinese ann. math. ser. b 44 (2023), 1-10").is_none_or(|v| v.name != "Annals of Mathematics"));
        assert_eq!(resolve("published in Ann. Math. 198 (2023)").map(|v| v.name), Some("Annals of Mathematics"));
    }

    #[test]
    fn parenthetical_notes_do_not_continue_the_title() {
        assert_eq!(
            resolve_comment("Accepted for publication in Physical Review Letters (Editors' Suggestion)").map(|v| v.name),
            Some("Physical Review Letters")
        );
    }

    #[test]
    fn negation_stops_at_commas() {
        assert_eq!(
            resolve_comment("This is not the final version, accepted for publication in Annals of Mathematics").map(|v| v.name),
            Some("Annals of Mathematics")
        );
        assert!(resolve_comment("Not accepted for publication in Annals of Mathematics").is_none());
    }

    #[test]
    fn trailing_remarks_after_a_comma_are_not_title_words() {
        assert_eq!(resolve_comment("To appear in Annals of Probability, revised version").map(|v| v.name), Some("Annals of Probability"));
        assert_eq!(resolve("Acta Mathematica; erratum 12 (2020)").map(|v| v.name), Some("Acta Mathematica"));
    }

    #[test]
    fn shortened_title_words_still_continue_the_title() {
        assert!(resolve("Acta Mathematica Sinica 40 (2024), 1-11").is_none_or(|v| v.name != "Acta Mathematica"));
        assert_eq!(resolve("J. Combin. Theory Ser. A 12 (2020)").map(|v| v.q), Some(1));
    }

    #[test]
    fn workshop_references_do_not_inherit_the_conference() {
        assert!(resolve("Proceedings of the Workshop on Foundation Models at ICML 2024").is_none_or(|v| v.name != "ICML"));
        assert_eq!(resolve("Proceedings of ICML 2024, pp. 1-10").map(|v| v.name), Some("ICML"));
    }

    #[test]
    fn nature_london_reference() {
        assert_eq!(resolve("Nature (London) 431, 162-167 (2004)").map(|v| (v.name, v.q)), Some(("Nature", 1)));
        assert_eq!(resolve("Nature 431, 162 (2004)").map(|v| v.name), Some("Nature"));
        assert!(resolve("The nature of space and time").is_none());
    }

    #[test]
    fn sentence_case_titles_are_continuations_too() {
        assert!(resolve("Acta mathematica vietnamica 44 (2019), 117-139").is_none_or(|v| v.name != "Acta Mathematica"));
        assert!(resolve("acta mathematica vietnamica 44 (2019), 117-139").is_none_or(|v| v.name != "Acta Mathematica"));
        assert_eq!(resolve("Acta Mathematica, to appear").map(|v| v.name), Some("Acta Mathematica"));
    }

    #[test]
    fn uppercase_titles_are_not_acronyms() {
        assert!(resolve("ACTA MATHEMATICA VIETNAMICA 44 (2019), 117-139").is_none_or(|v| v.name != "Acta Mathematica"));
    }

    #[test]
    fn full_subtitled_titles_resolve() {
        assert_eq!(resolve("Physica A: Statistical Mechanics and its Applications 500 (2018) 1-10").map(|v| (v.name, v.q)), Some(("Physica A", 2)));
        assert_eq!(resolve("Physica D: Nonlinear Phenomena 400 (2019)").map(|v| v.name), Some("Physica D"));
    }

    #[test]
    fn article_type_qualifiers_do_not_change_the_journal() {
        assert_eq!(resolve_comment("Accepted for publication in Physical Review B, Rapid Communications").map(|v| v.name), Some("Physical Review B"));
    }

    #[test]
    fn bibliographic_markers_are_not_title_words() {
        assert_eq!(resolve("Physical Review Letters, Volume 132, 050201 (2024)").map(|v| v.name), Some("Physical Review Letters"));
    }

    #[test]
    fn comment_titles_that_continue_are_other_journals() {
        assert!(resolve_comment("Published at Pattern Recognition Letters 2024").is_none_or(|v| v.name != "Pattern Recognition"));
        assert_eq!(resolve_comment("Accepted for publication in Pattern Recognition").map(|v| v.name), Some("Pattern Recognition"));
    }

    #[test]
    fn parenthesised_acronym_is_not_a_title_continuation() {
        assert_eq!(resolve("Journal of Machine Learning Research (JMLR) 24 (2023), 1-30").map(|v| v.name), Some("Journal of Machine Learning Research"));
    }

    #[test]
    fn longer_journal_titles_are_not_prefix_matched() {
        assert!(resolve("Acta Mathematica Vietnamica 44 (2019), 117-139").is_none_or(|v| v.name != "Acta Mathematica"));
        assert_eq!(resolve("Acta Math. 221 (2018)").map(|v| v.name), Some("Acta Mathematica"));
        assert_eq!(resolve("Annals of Mathematics. Second Series 167 (2008)").map(|v| v.name), Some("Annals of Mathematics"));
    }

    #[test]
    fn embedded_journal_names_are_not_matched() {
        assert!(resolve("Chinese Ann. Math. Ser. B 44 (2023), 1-10").is_none_or(|v| v.name != "Annals of Mathematics"));
        assert_eq!(resolve("Ann. Math. 198 (2023) 1-50").map(|v| v.name), Some("Annals of Mathematics"));
    }

    #[test]
    fn journal_prefix_of_conference_title_is_rejected() {
        assert!(resolve("Proceedings of the IEEE International Conference on Communications (ICC)").is_none_or(|v| v.name != "Proceedings of the IEEE"));
        assert_eq!(resolve("Proceedings of the IEEE 108 (2020) 1-20").map(|v| v.name), Some("Proceedings of the IEEE"));
        assert!(resolve_comment("Proceedings of the IEEE/CVF Winter Conference on Applications of Computer Vision (WACV 2024)").is_none_or(|v| v.name != "Proceedings of the IEEE"));
    }

    #[test]
    fn glued_year_is_split() {
        assert_eq!(resolve_comment("accepted in COCOON2025").map(|v| v.name), Some("COCOON"));
        assert_eq!(resolve_comment("To appear at ICML2024.").map(|v| v.name), Some("ICML"));
    }

    #[test]
    fn a_and_a_alias() {
        assert_eq!(resolve("A&A 650, A1 (2021)").map(|v| v.name), Some("Astronomy & Astrophysics"));
        assert_eq!(resolve_comment("accepted for publication in A&A Letter").map(|v| v.name), Some("Astronomy & Astrophysics"));
        assert!(resolve("Dept. of A&A report").is_none());
    }

    #[test]
    fn apj_alias_resolves() {
        assert_eq!(resolve("ApJ 900, 1 (2020)").map(|v| v.name), Some("The Astrophysical Journal"));
        assert_eq!(resolve_comment("Accepted to ApJ").map(|v| v.name), Some("The Astrophysical Journal"));
    }

    #[test]
    fn two_letter_acronyms() {
        for c in ["Accepted to CP 2024", "Accepted to KR 2024", "To appear in EC 2024"] {
            assert_eq!(resolve_comment(c).map(|v| v.q), Some(1), "{c}");
        }
        assert!(resolve_comment("CP violation in B decays, 12 pages").is_none());
    }

    #[test]
    fn explicit_short_aliases_resolve_from_comments() {
        let v = resolve_comment("Accepted to CoRL 2024").expect("CoRL resolves");
        assert_eq!((v.name, v.q), ("CoRL", 1));
    }
}

#[cfg(test)]
mod table_roundtrip {
    use super::*;

    /// Every bundled venue must resolve from its own full name written as a journal reference.
    #[test]
    fn every_name_resolves_to_itself() {
        let mut bad = Vec::new();
        for line in include_str!("../data/journals.tsv").lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let name = line.split('\t').next().unwrap_or("").trim_start_matches(['!', '^']);
            let r = resolve(&format!("{name} 12 (2020), 1-10")).map(|v| v.name);
            if r != Some(name) && !bad.iter().any(|(n, _): &(String, Option<&str>)| n == name) {
                bad.push((name.to_string(), r));
            }
        }
        assert!(bad.is_empty(), "{} names do not roundtrip: {bad:?}", bad.len());
    }
}
