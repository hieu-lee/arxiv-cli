//! Flag table (single source of truth for parsing AND --help), parser, shell-ish tokenizer.
//!
//! Conventions (documented once in help): comma inside one flag value = OR;
//! repeating a flag = AND.

pub struct Flag {
    pub name: &'static str,
    pub short: Option<char>,
    /// Metavar; empty string = boolean switch.
    pub arg: &'static str,
    pub group: &'static str,
    pub help: &'static str,
}

const fn f(name: &'static str, short: Option<char>, arg: &'static str, group: &'static str, help: &'static str) -> Flag {
    Flag { name, short, arg, group, help }
}

pub const FLAGS: &[Flag] = &[
    // ---- query (server side) ----
    f("ti", None, "T", "query", "title words"),
    f("abs", None, "T", "query", "abstract words"),
    f("kw", Some('k'), "T", "query", "keywords, match title+abstract (comma=OR)"),
    f("not", None, "T", "query", "exclude words (title+abstract)"),
    f("all", None, "T", "query", "any-field words"),
    f("co", None, "T", "query", "arXiv comment contains (server side)"),
    f("jr", None, "T", "query", "journal_ref contains (server side)"),
    f("au", Some('a'), "NAME", "query", "author, fuzzy, accent-insensitive (comma=OR, repeat=AND)"),
    f("cat", Some('c'), "C", "query", "category id/glob/name: math.CO, math.*, combinatorics"),
    f("xcat", None, "C", "query", "exclude category"),
    f("tag", Some('t'), "TAG", "query", "preset topic (see `arx tags`)"),
    f("since", None, "D", "query", "from date: 2024, 2024-03, 2024-03-05, 30d, 6m, 2y"),
    f("until", None, "D", "query", "to date (same formats)"),
    f("upd", None, "", "query", "since/until apply to last-update date, not submission"),
    f("id", None, "ID", "query", "arXiv ids (comma list)"),
    f("sort", Some('s'), "S", "query", "rel|new|old|upd (default rel; new when no text)"),
    f("n", Some('n'), "N", "query", "max results to return (default 10)"),
    f("skip", None, "N", "query", "skip first N results (text: skip N chars)"),
    f("scan", None, "N", "query", "max raw rows fetched when post-filtering (default 400)"),
    // ---- post filters (client side) ----
    f("inst", Some('i'), "ORG", "filter", "author institution, fuzzy (fetches full text; comma=OR)"),
    f("published", None, "", "filter", "has journal_ref or DOI"),
    f("preprint", None, "", "filter", "no journal_ref, no DOI, no accepted-comment"),
    f("accepted", None, "", "filter", "published OR comment says accepted/to appear"),
    f("q", None, "Q", "filter", "venue quartile: 1 | 1,2 | 1-2 (from journal_ref or 'accepted to X' comment)"),
    f("jn", None, "J", "filter", "journal name fuzzy match on journal_ref"),
    f("cmt", None, "T", "filter", "comment contains (case-insens, client side)"),
    f("pages", None, "R", "filter", "page count from comment: 20 | 10- | -30 | 10-30"),
    f("nau", None, "R", "filter", "author count range"),
    f("msc", None, "M", "filter", "MSC class prefix, e.g. 11,05C (fetches abs page)"),
    f("pcat", None, "C", "filter", "primary category only (comma=OR, globs ok)"),
    f("doi", None, "", "filter", "has DOI"),
    f("fuzz", None, "X", "filter", "fuzzy threshold 0-1 (default 0.82)"),
    // ---- output ----
    f("fmt", None, "F", "output", "l (default) | tsv | json | jsonl | ids | bib | md"),
    f("fields", Some('f'), "F", "output", "id,ver,t,a,aff,inst,d,u,c,pc,ab,co,jr,doi,pub,q,v,msc,url,pdf"),
    f("abslen", None, "N", "output", "truncate abstract to N chars (default 280 when shown)"),
    f("aumax", None, "N", "output", "show first N authors then +K (default 3)"),
    f("tok", None, "N", "output", "hard output budget in tokens (~4 chars/token)"),
    f("facet", None, "F", "output", "count by cat,pcat,year,month,au,q,venue,pub,inst instead of listing"),
    f("full", None, "", "output", "no author/abstract truncation"),
    // ---- paper content (text/outline/sec/grep/prob/refs/tex) ----
    f("kind", None, "K", "content", "prob,conj,q,thm,lem,prop,cor,def,rem,exm,cl,ex,all (default prob,conj,q)"),
    f("sec", None, "S", "content", "section id (1.2) or title substring"),
    f("pat", Some('g'), "RE", "content", "grep pattern (case-insens substring; /re/ for regex-lite)"),
    f("ctx", None, "N", "content", "context chars around grep hit (default 120)"),
    f("max", Some('m'), "N", "content", "max items/hits per paper"),
    f("src", None, "S", "content", "html (default) | tex | pdf (dl only)"),
    f("raw", None, "", "content", "text/sec with --src tex: flattened LaTeX source (includes inlined, comments dropped) instead of cleaned text"),
    f("to", None, "PATH", "content", "write fetched file(s) to dir (dl)"),
    // ---- engine ----
    f("par", Some('j'), "N", "engine", "max concurrent www fetches (default 12)"),
    f("rate", None, "MS", "engine", "min gap between API calls in ms (default 3000, arXiv policy)"),
    f("ttl", None, "S", "engine", "cache TTL seconds (default 3600 api / 604800 papers)"),
    f("fresh", None, "", "engine", "bypass cache reads"),
    f("keep", None, "", "engine", "pipeline: also print this stage's output"),
    f("peek", None, "", "engine", "alerts: do not advance last-checked date"),
    f("qfile", None, "PATH", "engine", "extra venue TSV: name<TAB>q<TAB>aliases,;-separated"),
    f("help", Some('h'), "", "engine", "help"),
];

#[derive(Debug, Default, Clone)]
pub struct Args {
    pub pos: Vec<String>,
    /// (flag name, raw value) in order of appearance; booleans carry "".
    pub kv: Vec<(&'static str, String)>,
}

impl Args {
    pub fn flag(&self, name: &str) -> bool {
        self.kv.iter().any(|(k, _)| *k == name)
    }
    /// Last occurrence.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.kv.iter().rev().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }
    /// All raw occurrences (each one is an AND group).
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.kv.iter().filter(|(k, _)| *k == name).map(|(_, v)| v.as_str()).collect()
    }
    /// Each occurrence split on commas (trimmed, empties dropped): outer = AND, inner = OR.
    pub fn groups(&self, name: &str) -> Vec<Vec<String>> {
        self.all(name)
            .into_iter()
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>())
            .filter(|g| !g.is_empty())
            .collect()
    }
    /// All comma items across occurrences flattened (OR semantics).
    pub fn list(&self, name: &str) -> Vec<String> {
        self.groups(name).into_iter().flatten().collect()
    }
    pub fn num<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.get(name).and_then(|v| v.trim().parse().ok())
    }
    pub fn has_any(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.flag(n))
    }
}

fn lookup_long(name: &str) -> Option<&'static Flag> {
    FLAGS.iter().find(|f| f.name == name)
}
fn lookup_short(c: char) -> Option<&'static Flag> {
    FLAGS.iter().find(|f| f.short == Some(c))
}

fn suggest(name: &str) -> String {
    let mut best: Option<(&str, usize)> = None;
    for f in FLAGS {
        let d = lev(name, f.name);
        if best.map_or(true, |(_, bd)| d < bd) {
            best = Some((f.name, d));
        }
    }
    match best {
        Some((n, d)) if d <= 2 || n.starts_with(name) || name.starts_with(n) => format!(" (did you mean --{n}?)"),
        _ => String::new(),
    }
}

fn lev(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let c = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + c);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Lenient scan of every flag occurrence with its value, honouring `--`, `--x=v`, `-n5` and which flags
/// take values (used to find engine flags without running the real parser). Operators and unknown
/// flags are skipped.
pub fn scan(tokens: &[String]) -> Vec<(&'static Flag, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        i += 1;
        if t == "--" {
            // statement boundary resets nothing: `--` only ends flags of its own stage
            while i < tokens.len() && tokens[i] != OP_SEMI && tokens[i] != OP_PIPE {
                i += 1;
            }
            continue;
        }
        if t == "-" || !t.starts_with('-') || is_negative_number(t) {
            continue;
        }
        let (flag, inline) = if let Some(long) = t.strip_prefix("--") {
            let (n, v) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            match lookup_long(n) {
                Some(f) => (f, v),
                None => continue,
            }
        } else {
            let mut cs = t[1..].chars();
            let Some(f) = cs.next().and_then(lookup_short) else { continue };
            let rest: String = cs.collect();
            (f, (!rest.is_empty()).then(|| rest.trim_start_matches('=').to_string()))
        };
        if flag.arg.is_empty() {
            out.push((flag, String::new()));
        } else if let Some(v) = inline {
            out.push((flag, v));
        } else if i < tokens.len() && tokens[i] != OP_SEMI && tokens[i] != OP_PIPE {
            out.push((flag, tokens[i].clone()));
            i += 1;
        }
    }
    out
}

/// Parse tokens (without the program name / command word) into Args.
/// Supports --flag v, --flag=v, -n 5, -n5, `--` to end flags.
pub fn parse(tokens: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut i = 0;
    let mut only_pos = false;
    while i < tokens.len() {
        let t = &tokens[i];
        i += 1;
        if only_pos || t == "-" || !t.starts_with('-') || is_negative_number(t) {
            a.pos.push(t.clone());
            continue;
        }
        if t == "--" {
            only_pos = true;
            continue;
        }
        let (flag, inline): (&'static Flag, Option<String>) = if let Some(long) = t.strip_prefix("--") {
            let (name, val) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let fl = lookup_long(name).ok_or_else(|| format!("unknown flag --{name}{}", suggest(name)))?;
            (fl, val)
        } else {
            let mut cs = t[1..].chars();
            let c = cs.next().unwrap();
            let fl = lookup_short(c).ok_or_else(|| format!("unknown flag -{c}"))?;
            let rest: String = cs.collect();
            (fl, if rest.is_empty() { None } else { Some(rest.trim_start_matches('=').to_string()) })
        };
        if flag.arg.is_empty() {
            if let Some(v) = inline {
                return Err(format!("--{} takes no value (got {v:?})", flag.name));
            }
            a.kv.push((flag.name, String::new()));
        } else {
            let v = match inline {
                Some(v) => v,
                None => {
                    let v = tokens.get(i).ok_or_else(|| format!("--{} needs {}", flag.name, flag.arg))?;
                    i += 1;
                    v.clone()
                }
            };
            a.kv.push((flag.name, v));
        }
    }
    Ok(a)
}

fn is_negative_number(t: &str) -> bool {
    t.len() > 1 && t.starts_with('-') && t[1..].chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Sentinel tokens for UNQUOTED control operators (quoted `;`/`|` stay ordinary text).
pub const OP_SEMI: &str = "\u{1};";
pub const OP_PIPE: &str = "\u{1}|";

/// Shell-like split: whitespace separated, '..' and ".." quoting, backslash escapes inside "..".
/// Unquoted `;`, `|`, newline come back as `OP_SEMI` / `OP_PIPE` tokens.
pub fn tokenize(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                have = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err("unterminated '".into()),
                    }
                }
            }
            '"' => {
                have = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\')) => cur.push(ch),
                            Some(ch) => {
                                cur.push('\\');
                                cur.push(ch)
                            }
                            None => return Err("unterminated \"".into()),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err("unterminated \"".into()),
                    }
                }
            }
            ';' | '|' | '\n' => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
                out.push(if c == '|' { OP_PIPE } else { OP_SEMI }.to_string());
            }
            c if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                have = true;
                cur.push(c);
            }
        }
    }
    if have {
        out.push(cur);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }
    #[test]
    fn parses_mixed() {
        let a = parse(&v(&["diffusion", "-n5", "--cat=math.CO,math.NT", "--au", "tao", "--au", "green", "--published"])).unwrap();
        assert_eq!(a.pos, ["diffusion"]);
        assert_eq!(a.num::<usize>("n"), Some(5));
        assert_eq!(a.list("cat"), ["math.CO", "math.NT"]);
        assert_eq!(a.groups("au").len(), 2);
        assert!(a.flag("published"));
    }
    #[test]
    fn raw_is_a_switch() {
        let a = parse(&["ID".to_string(), "--raw".into(), "--src".into(), "tex".into()]).unwrap();
        assert!(a.flag("raw"));
        assert_eq!(a.get("src"), Some("tex"));
        assert_eq!(a.pos, ["ID"]);
    }

    #[test]
    fn unknown_flag_suggests() {
        let e = parse(&v(&["--sinc", "2020"])).unwrap_err();
        assert!(e.contains("--since"), "{e}");
    }
    #[test]
    fn tokenizer_ops() {
        assert_eq!(tokenize("search 'a b' -n 3 | prob; get 1").unwrap(), ["search", "a b", "-n", "3", OP_PIPE, "prob", OP_SEMI, "get", "1"]);
        assert_eq!(tokenize("cats \"|\" ';'").unwrap(), ["cats", "|", ";"]);
    }
    #[test]
    fn negative_number_positional() {
        assert_eq!(parse(&v(&["-5"])).unwrap().pos, ["-5"]);
    }
}
