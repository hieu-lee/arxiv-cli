//! Dispatcher + batch executor: `a ; b` run concurrently, `a | b` chains papers into the next stage.

use futures_util::future::join_all;
use std::fmt::Write;
use std::sync::Arc;

use crate::args::{self, Args};
use crate::cmd::{self, Input, Out};
use crate::net::Net;
use crate::{help, store, venue};

const CMDS: &[(&str, &[&str])] = &[
    ("search", &["s", "find", "q"]),
    ("get", &["g", "show", "abs", "info"]),
    ("bib", &["bibtex", "cite"]),
    ("com", &["comment", "comments"]),
    ("prob", &["problem", "problems", "p"]),
    ("text", &["read", "md"]),
    ("outline", &["o", "toc"]),
    ("sec", &["section"]),
    ("grep", &["find-in", "gr"]),
    ("refs", &["ref", "references"]),
    ("cites", &["citations", "citing"]),
    ("ver", &["versions", "v"]),
    ("cats", &["cat", "categories"]),
    ("tags", &["tag", "topics"]),
    ("venue", &["venues", "journal", "jq"]),
    ("related", &["similar", "rel"]),
    ("dl", &["download", "fetch"]),
    ("watch", &[]),
    ("unwatch", &[]),
    ("watches", &[]),
    ("alerts", &["alert"]),
    ("cache", &[]),
    ("x", &["run", "batch"]),
    ("help", &[]),
];

pub fn canonical(word: &str) -> Option<&'static str> {
    CMDS.iter().find(|(c, al)| *c == word || al.contains(&word)).map(|(c, _)| *c)
}

/// Read all of stdin (used once, by the top-level `x -`).
pub fn read_stdin() -> Result<String, String> {
    use std::io::Read;
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).map_err(|e| format!("stdin: {e}"))?;
    Ok(s)
}

/// Script of an `x` command: its arguments minus outer engine flags (`--fresh`, `--par 4`, ...), which belong
/// to the engine rather than to the script text.
fn script_text(rest: &[String]) -> String {
    let mut script: Vec<&str> = Vec::new();
    let mut it = rest.iter();
    while let Some(t) = it.next() {
        // everything after an explicit `--` is script text, whatever it looks like
        if t.as_str() == "--" {
            script.extend(it.by_ref().map(|s| s.as_str()));
            break;
        }
        // `--qfile=PATH` is an option even when PATH contains spaces; any other argument with whitespace
        // is a script string
        if is_qfile_option(t) {
            continue;
        }
        if t.contains(char::is_whitespace) {
            script.push(t);
            continue;
        }
        let (name, inline) = match t.strip_prefix("--") {
            Some(r) => match r.split_once('=') {
                Some((n, _)) => (n, true),
                None => (r, false),
            },
            // -j2 / -j=2 / -j 2
            None if t.starts_with("-j") => ("par", t.len() > 2),
            None => ("", false),
        };
        match name {
            "fresh" => {}
            "par" | "rate" | "ttl" | "qfile" | "via" => {
                if !inline {
                    it.next();
                }
            }
            _ => script.push(t),
        }
    }
    script.join("\n")
}

/// Command word + remaining args; no command word means `get` (all ids) or `search`.
fn split_cmd(toks: &[String]) -> (&'static str, &[String]) {
    match toks.first().and_then(|w| canonical(w)) {
        Some(c) => (c, &toks[1..]),
        None => {
            let all_ids = !toks.is_empty() && toks.iter().all(|t| !t.starts_with('-') && cmd::looks_like_id(t));
            (if all_ids { "get" } else { "search" }, toks)
        }
    }
}

/// Execute one stage. `toks` = command word + args.
pub async fn stage(net: &Arc<Net>, toks: &[String], input: Input) -> Result<Out, String> {
    let (cmd, rest) = split_cmd(toks);
    if cmd == "x" {
        // statement strings are opaque here (they may start with flags); `-` reads stdin
        let after_dashes = rest.iter().skip_while(|s| s.as_str() != "--").skip(1);
        if rest.iter().take_while(|s| s.as_str() != "--").chain(after_dashes).any(|s| s.as_str() == "-") {
            // the top-level `x -` is expanded in main (before engine flags are collected)
            return Err("stdin `-` is only valid as a direct argument of the top-level x".into());
        }
        let src = script_text(rest);
        let toks = args::tokenize(&src)?;
        let (text, ok, cursors) = run_batch(net, &toks).await;
        return Ok(Out { text, failed: usize::from(!ok), cursors, ..Default::default() });
    }
    let mut a = args::parse(rest)?;
    if a.flag("help") {
        return Ok(Out { papers: vec![], text: help::topic(cmd), ..Default::default() });
    }
    match cmd {
        "search" => cmd::search(net, &a, input).await,
        "get" => cmd::get(net, &a, input).await,
        "bib" => {
            a.kv.push(("fmt", "bib".into()));
            cmd::search(net, &a, input).await
        }
        "com" => cmd::comments(net, &a, input).await,
        "prob" => cmd::problems(net, &a, input).await,
        "text" => cmd::text(net, &a, input).await,
        "outline" => cmd::outline(net, &a, input).await,
        "sec" => {
            // `sec ID KEY`: last non-id positional becomes --sec
            if !a.flag("sec") {
                if let Some(k) = a.pos.iter().rposition(|s| !cmd::looks_like_id(s)) {
                    let key = a.pos.remove(k);
                    a.kv.push(("sec", key));
                }
            }
            cmd::text(net, &a, input).await
        }
        "grep" => cmd::grep(net, &a, input).await,
        "refs" => cmd::refs(net, &a, input).await,
        "cites" => cmd::cites(net, &a, input).await,
        "ver" => cmd::versions(net, &a, input).await,
        "related" => cmd::related(net, &a, input).await,
        "dl" => cmd::download(net, &a, input).await,
        "cats" => cmd::cats(&a),
        "tags" => cmd::tag_list(&a),
        "venue" => cmd::venue_cmd(&a),
        "watch" => watch(&a, rest),
        "unwatch" => unwatch(&a),
        "watches" => watches(),
        "alerts" => alerts(net, &a).await,
        "cache" => match a.pos.first().map(String::as_str) {
            Some("clear") => store::cache_clear().map(text_out),
            _ => Ok(text_out(store::cache_stats())),
        },
        "help" => {
            let w = a.pos.first().map(String::as_str).unwrap_or("");
            Ok(text_out(help::topic(if w == "flags" { w } else { canonical(w).unwrap_or("") })))
        }
        _ => unreachable!(),
    }
}

fn text_out(t: impl Into<String>) -> Out {
    Out { text: t.into(), ..Default::default() }
}

fn watch(a: &Args, rest: &[String]) -> Result<Out, String> {
    let name = a.pos.first().ok_or("watch: arx watch NAME <search args>")?.clone();
    // everything after the NAME token is the saved query
    let at = rest.iter().position(|t| *t == name).unwrap_or(0);
    let q = store::join_tokens(&rest[at + 1..]);
    if q.is_empty() {
        return Err("watch: give search args after NAME".into());
    }
    store::update_watches(|w| {
        w[&name] = serde_json::json!({"q": q, "last": "", "seen": []});
        Ok(())
    })?;
    Ok(text_out(format!("watching {name}: {q}")))
}

fn unwatch(a: &Args) -> Result<Out, String> {
    let name = a.pos.first().ok_or("unwatch: give NAME")?;
    let removed = store::update_watches(|w| Ok(w.as_object_mut().and_then(|m| m.remove(name.as_str())).is_some()))?;
    Ok(text_out(if removed { format!("removed {name}") } else { format!("{name}: not found") }))
}

fn watches() -> Result<Out, String> {
    let w = store::load_watches()?;
    let Some(m) = w.as_object().filter(|m| !m.is_empty()) else {
        return Ok(text_out("no watches"));
    };
    let mut out = String::new();
    for (k, v) in m {
        if !out.is_empty() {
            out.push('\n');
        }
        let last = v["last"].as_str().filter(|s| !s.is_empty()).unwrap_or("never");
        let _ = write!(out, "{k} [{last}] {}", v["q"].as_str().unwrap_or(""));
    }
    Ok(text_out(out))
}

/// First-run window for a watch that was never checked.
const ALERT_FIRST_WINDOW: &str = "30d";
/// Raw rows scanned per alert run when the watch sets no --scan (so selective filters cannot stall a cursor).
const ALERT_SCAN: usize = 2000;

/// New papers per watch since its cursor (date of the newest paper already shown, plus ids shown on that
/// date). Oldest-first with a cap so truncated runs resume exactly where they stopped.
use crate::cmd::Cursor;

/// Persist alert cursors. Called by main once the output has been written (and only for statements that
/// succeeded), so a failed stage or write never consumes undelivered papers.
pub fn commit_cursors(cursors: Vec<Cursor>) -> Result<(), String> {
    if cursors.is_empty() {
        return Ok(());
    }
    store::update_watches(|w| {
            for (n, (d, ids), (q0, last0)) in &cursors {
                // skip if the watch was replaced or advanced while the request was in flight
                let stored_last = w[n.as_str()]["last"].as_str().unwrap_or("").to_string();
                let same = w[n.as_str()]["q"].as_str() == Some(q0.as_str()) && (stored_last == *last0 || stored_last == *d);
                if same {
                    // same-day progress from a concurrent run is merged, never replaced
                    let mut all: Vec<String> = ids.clone();
                    if stored_last == *d {
                        for s in w[n.as_str()]["seen"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
                            if !all.iter().any(|x| x == s) {
                                all.push(s.to_string());
                            }
                        }
                    }
                    w[n.as_str()]["last"] = serde_json::json!(d);
                    w[n.as_str()]["seen"] = serde_json::json!(all);
                }
            }
            Ok(())
    })
}

async fn alerts(net: &Arc<Net>, a: &Args) -> Result<Out, String> {
    let w = store::load_watches()?;
    let names: Vec<String> = if a.pos.is_empty() { w.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default() } else { a.pos.clone() };
    if names.is_empty() {
        return Ok(text_out("no watches"));
    }
    let cap = a.num::<usize>("n").unwrap_or(100).clamp(1, crate::engine::MAX_ROWS);
    // venue tables saved with the watches are loaded once, up front (the table is process-global, so
    // loading inside concurrent searches would let one watch change another's classification)
    for n in &names {
        if let Some(q) = w[n.as_str()]["q"].as_str() {
            load_qfiles(&engine_flags(&args::tokenize(q)?))?;
        }
    }
    let futs = names.iter().map(|n| {
        let spec = w[n.as_str()].clone();
        async move {
            if spec.is_null() {
                return (n.clone(), Err(format!("{n}: not found")), None::<((String, Vec<String>), (String, String))>, Vec::<crate::model::Paper>::new());
            }
            let seen: Vec<String> = spec["seen"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let last = spec["last"].as_str().unwrap_or("").to_string();
            let saved = match args::tokenize(spec["q"].as_str().unwrap_or("")).and_then(|t| args::parse(&t)) {
                Ok(x) => x,
                Err(e) => return (n.clone(), Err(e), None, Vec::new()),
            };
            let by_update = saved.flag("upd");
            let want = cap + seen.len();
            // alert overrides go straight into the parsed flags (a saved `--` cannot swallow them)
            let mut sa = saved;
            let since = if last.is_empty() { ALERT_FIRST_WINDOW.to_string() } else { last.clone() };
            sa.kv.push(("since", since));
            sa.kv.push(("sort", (if by_update { "uold" } else { "old" }).to_string()));
            sa.kv.push(("n", want.to_string()));
            if !sa.flag("scan") {
                sa.kv.push(("scan", ALERT_SCAN.to_string()));
            }
            // the cursor must cover exactly what is shown: no token-budget clipping, no saved paging offset
            sa.kv.retain(|(k, _)| *k != "tok" && *k != "skip");
            let r = match crate::engine::search(net, &sa).await {
                Ok(o) => o,
                Err(e) => return (n.clone(), Err(e), None, Vec::new()),
            };
            let scan_to = r.scan_to.clone();
            let raw = r.papers.len();
            let day = |p: &crate::model::Paper| if by_update { p.updated_date().to_string() } else { p.date().to_string() };
            // `seen` ids only suppress papers on the cursor day (an updated paper moves to a later day)
            let fresh: Vec<_> = r.papers.into_iter().filter(|p| !(day(p) == last && seen.contains(&p.id))).take(cap).collect();
            let more = raw >= want && !fresh.is_empty();
            let mut fresh = fresh;
            if let Err(e) = cmd::enrich_for_display(net, &sa, &mut fresh).await {
                return (n.clone(), Err(e), None, Vec::new()); // cursor stays put: nothing was reported
            }
            let text = cmd::render(&sa, &fresh, None);
            // the scan limit was hit without leaving the cursor day: skip to the next day (visibly) so the watch cannot stall
            let mut skipped = None;
            let scan_to = scan_to.map(|d| if !last.is_empty() && d == last { skipped = Some(next_day(&d)); d } else { d });
            let cursor = fresh.iter().map(&day).max().or_else(|| skipped.clone().or_else(|| scan_to.filter(|d| d.as_str() > last.as_str()))).map(|d| {
                let mut ids: Vec<String> = fresh.iter().filter(|p| day(p) == d).map(|p| p.id.clone()).collect();
                if d == last {
                    ids.extend(seen.iter().cloned());
                }
                (d, ids)
            });
            let snap = (spec["q"].as_str().unwrap_or("").to_string(), last);
            let note = if skipped.is_some() && fresh.is_empty() { "\n(scan limit hit within one day: skipped to the next day; use a tighter watch)" } else { "" };
            (n.clone(), text.map(|t| if more { format!("{t}\n(more pending: rerun alerts)") } else { format!("{t}{note}") }), cursor.map(|c| (c, snap)), fresh)
        }
    });
    let results = join_all(futs).await;
    let mut s = String::new();
    let mut failed = 0;
    let mut cursors = Vec::new();
    let mut shown: Vec<crate::model::Paper> = Vec::new();
    for (n, r, cur, ps) in results {
        match r {
            Ok(t) => {
                s.push_str(&format!("#{n}\n{t}\n"));
                shown.extend(ps);
                if let (Some((c, snap)), false) = (cur, a.flag("peek")) {
                    cursors.push((n, c, snap));
                }
            }
            Err(e) => {
                failed += 1;
                s.push_str(&format!("#{n} error: {e}\n"));
            }
        }
    }
    // stale or substitute data (cache past its TTL, OpenAlex) may lack papers: never advance a cursor over it
    let cursors = if net.degraded() {
        if !cursors.is_empty() {
            s.push_str("(alert cursors not advanced: results came from stale or fallback sources)\n");
        }
        Vec::new()
    } else {
        cursors
    };
    Ok(Out { papers: shown, text: s.trim_end().to_string(), failed, cursors, ..Default::default() })
}

/// "2026-10-23" -> "2026-10-24"
fn next_day(d: &str) -> String {
    let p: Vec<i64> = d.split('-').filter_map(|x| x.parse().ok()).collect();
    match p[..] {
        [y, m, dd] => {
            let (y, m, dd) = crate::dates::civil_from_days(crate::dates::days_from_civil(y, m as u32, dd as u32) + 1);
            format!("{y:04}-{m:02}-{dd:02}")
        }
        _ => d.to_string(),
    }
}

fn split_on<'a>(toks: &'a [String], sep: &str) -> Vec<&'a [String]> {
    let mut v = Vec::new();
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        if t == sep {
            v.push(&toks[start..i]);
            start = i + 1;
        }
    }
    v.push(&toks[start..]);
    v.retain(|s| !s.is_empty());
    v
}

/// Every stage (including those of nested `x` scripts) is a command that never touches the network.
pub fn all_local(toks: &[String]) -> bool {
    const LOCAL: &[&str] = &["cats", "tags", "venue", "watch", "unwatch", "watches", "cache", "help"];
    split_on(toks, args::OP_SEMI).iter().all(|st| {
        split_on(st, args::OP_PIPE).iter().all(|sg| match split_cmd(sg) {
            ("x", rest) => args::tokenize(&script_text(rest)).is_ok_and(|t| all_local(&t)),
            (c, _) => LOCAL.contains(&c),
        })
    })
}

/// `--qfile=PATH` as an outer option (the path may contain spaces) as opposed to a script that merely starts
/// with it (`--qfile=x.tsv --cat math.CO`): the remainder must not look like further flags or operators.
fn is_qfile_option(t: &str) -> bool {
    t.starts_with("--qfile=") && !t.split_whitespace().skip(1).any(|w| w.starts_with('-') || w == ";" || w == "|")
}

/// Outer tokens of an `x` command that can be options: whitespace-free tokens plus the value (which may contain
/// spaces, e.g. a path) of a value-taking engine option; script strings are skipped.
fn outer_tokens(toks: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = toks.iter();
    while let Some(t) = it.next() {
        if t.as_str() == "--" {
            break; // the rest is script text
        }
        if is_qfile_option(t) {
            out.push(t.clone());
            continue;
        }
        if t.contains(char::is_whitespace) {
            continue;
        }
        out.push(t.clone());
        let takes_value = matches!(t.as_str(), "--par" | "--rate" | "--ttl" | "--qfile" | "--via" | "-j");
        if takes_value {
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        }
    }
    out
}

/// Engine flags (--par/--rate/--ttl/--fresh) found anywhere in the command line, including inside
/// the statement string of `x`, so the shared HTTP engine honours them.
pub fn engine_flags(toks: &[String]) -> Args {
    let mut found = Vec::new();
    collect_flags(toks, 0, &mut found);
    let mut a = Args::default();
    for (f, v) in found {
        if matches!(f.name, "fresh" | "par" | "rate" | "ttl" | "qfile" | "via") {
            a.kv.push((f.name, v));
        }
    }
    a
}

/// Scan flags of `toks`; statements of every (possibly nested) `x` batch are scanned on their own so an
/// outer `--` or a quoted script cannot hide them.
fn collect_flags(toks: &[String], depth: usize, out: &mut Vec<(&'static args::Flag, String)>) {
    for stmt in split_on(toks, args::OP_SEMI) {
        for stage in split_on(stmt, args::OP_PIPE) {
            if stage.first().and_then(|w| canonical(w)) != Some("x") {
                out.extend(args::scan(stage));
                continue;
            }
            // `x`: its arguments are scripts, so only whitespace-free tokens can be outer options
            out.extend(args::scan(&outer_tokens(stage)));
            if depth >= 8 {
                continue;
            }
            // after an explicit `--` every argument is script text, even one that looks like --qfile=PATH
            let dash = stage.iter().position(|t| t == "--").unwrap_or(usize::MAX);
            for (k, t) in stage.iter().enumerate().skip(1) {
                if k < dash && is_qfile_option(t) {
                    continue;
                }
                if let Ok(inner) = args::tokenize(t) {
                    collect_flags(&inner, depth + 1, out);
                }
            }
        }
    }
}

/// Load every `--qfile` found on the command line once, before any concurrent work starts.
pub fn load_qfiles(a: &Args) -> Result<(), String> {
    for p in a.all("qfile") {
        let t = std::fs::read_to_string(p).map_err(|e| format!("--qfile {p}: {e}"))?;
        venue::load_extra(&t)?;
    }
    Ok(())
}

/// Run statements (`;`) concurrently, each a `|` chain. Returns (text, all_ok).
pub async fn run_batch(net: &Arc<Net>, toks: &[String]) -> (String, bool, Vec<Cursor>) {
    let stmts: Vec<Vec<&[String]>> = split_on(toks, args::OP_SEMI).into_iter().map(|st| split_on(st, args::OP_PIPE)).collect();
    // `x` is a batch runner, not a papers stage: keep pipelines inside one script
    for stages in &stmts {
        if stages.len() > 1 && stages.iter().any(|s| split_cmd(s).0 == "x") {
            return ("error: `x` cannot be piped; write the whole pipeline inside one x script".into(), false, Vec::new());
        }
    }
    let multi = stmts.len() > 1;
    let futs = stmts.iter().map(|stages| async move {
        let mut input: Input = None;
        let mut text = String::new();
        let mut ok = true;
        let mut cursors: Vec<Cursor> = Vec::new();
        let last = stages.len().saturating_sub(1);
        for (i, sg) in stages.iter().enumerate() {
            match stage(net, sg, input.take()).await {
                Ok(o) => {
                    ok &= o.failed == 0;
                    // a stage that only partly succeeded (a document could not be fetched) means what the
                    // upstream alerts reported was not fully delivered: its cursors must not advance
                    if o.failed > 0 {
                        cursors.clear();
                    }
                    // only a final `alerts` consumes its papers: a downstream stage (`| get --tok 1`) may show
                    // only part of what alerts returned, so `alerts | stage` is a preview that never advances
                    if i == last {
                        cursors.extend(o.cursors);
                    }
                    let keep = i == last || sg.iter().any(|t| t == "--keep");
                    if keep {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&o.text);
                    }
                    input = Some(o.papers);
                }
                // a failed stage drops the statement's cursors: nothing it computed was delivered
                Err(e) => return (format!("error: {e}"), false, Vec::new()),
            }
        }
        (text, ok, cursors)
    });
    let res = join_all(futs).await;
    let ok = res.iter().all(|(_, ok, _)| *ok);
    let cursors: Vec<Cursor> = res.iter().flat_map(|(_, _, c)| c.iter().cloned()).collect();
    let text = if multi {
        res.iter().enumerate().map(|(i, (t, _, _))| format!("#{}\n{}", i + 1, t)).collect::<Vec<_>>().join("\n")
    } else {
        res.into_iter().next().map(|(t, _, _)| t).unwrap_or_default()
    };
    (text, ok, cursors)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(s: &str) -> Vec<String> {
        args::tokenize(s).unwrap()
    }
    #[test]
    fn aliases() {
        assert_eq!(canonical("s"), Some("search"));
        assert_eq!(canonical("problems"), Some("prob"));
        assert_eq!(canonical("nope"), None);
    }
    #[test]
    fn local_commands_are_recognised() {
        assert!(all_local(&t("cats math ; venue annals | tags ; help x")));
        assert!(all_local(&["x".into(), "cats ; x \"tags\"".into()]));
        assert!(!all_local(&t("cats ; get 2301.00001")));
        assert!(!all_local(&t("search diffusion")));
        assert!(!all_local(&t("alerts")));
        assert!(!all_local(&["x".into(), "cats ; alerts".into()]));
    }

    #[test]
    fn local_batches_finish_in_one_poll() {
        use std::future::Future;
        let net = Arc::new(Net::for_test(&std::env::temp_dir(), &[]));
        let toks = t("cats math ; tags ; venue annals | cats ; help x ; x 'venue nature'");
        let mut fut = std::pin::pin!(run_batch(&net, &toks));
        let std::task::Poll::Ready((text, ok, _)) = fut.as_mut().poll(&mut std::task::Context::from_waker(std::task::Waker::noop())) else {
            panic!("a local batch must not wait on a runtime");
        };
        assert!(ok, "{text}");
        assert!(text.contains("math: AC Commutative Algebra"), "{text}");
        assert!(text.contains("Q1 Nature [phys]"), "{text}");
    }

    #[test]
    fn next_day_rolls_over() {
        assert_eq!(next_day("2026-10-23"), "2026-10-24");
        assert_eq!(next_day("2024-02-28"), "2024-02-29");
        assert_eq!(next_day("2026-12-31"), "2027-01-01");
    }
    #[test]
    fn splits() {
        let toks = t("search a | prob ; get b");
        let st = split_on(&toks, args::OP_SEMI);
        assert_eq!(st.len(), 2);
        assert_eq!(split_on(st[0], args::OP_PIPE).len(), 2);
        assert_eq!(split_on(&t(";;"), args::OP_SEMI).len(), 0);
        // quoted operators are plain arguments
        assert_eq!(split_on(&t("grep ID '|'"), args::OP_PIPE).len(), 1);
    }
    #[test]
    fn outer_engine_flags_are_not_script_text() {
        let rest: Vec<String> = ["cats math.CO", "--fresh", "--par=1", "--ttl", "0", "--au tao -n1"].iter().map(|s| s.to_string()).collect();
        assert_eq!(script_text(&rest), "cats math.CO\n--au tao -n1");
        let j: Vec<String> = ["cats", "-j1", "-j", "3"].iter().map(|s| s.to_string()).collect();
        assert_eq!(script_text(&j), "cats");
    }

    #[test]
    fn nested_scripts_are_not_outer_options() {
        let a = engine_flags(&["x".into(), "cats math.CO ; x \"--qfile=data/j.tsv --help\"".into()]);
        assert_eq!(a.get("qfile"), Some("data/j.tsv"));
        assert_eq!(a.all("qfile").len(), 1);
    }

    #[test]
    fn explicit_dashdash_keeps_scripts_verbatim() {
        let rest: Vec<String> = ["--fresh", "--", "--qfile=data/j.tsv diffusion"].iter().map(|s| s.to_string()).collect();
        assert_eq!(script_text(&rest), "--qfile=data/j.tsv diffusion");
        let a = engine_flags(&["x".into(), "--fresh".into(), "--".into(), "--qfile=data/j.tsv diffusion".into()]);
        assert!(a.flag("fresh"));
        assert_eq!(a.get("qfile"), Some("data/j.tsv"));
    }

    #[test]
    fn qfile_option_versus_script() {
        assert!(is_qfile_option("--qfile=/some dir/q.tsv"));
        assert!(!is_qfile_option("--qfile=data/j.tsv --cat math.CO -n1"));
        let rest: Vec<String> = ["--qfile=data/j.tsv --cat math.CO -n1 ; cats"].iter().map(|s| s.to_string()).collect();
        assert_eq!(script_text(&rest), "--qfile=data/j.tsv --cat math.CO -n1 ; cats");
        let a = engine_flags(&["x".into(), "--qfile=data/j.tsv --cat math.CO".into()]);
        assert_eq!(a.get("qfile"), Some("data/j.tsv"));
    }

    #[test]
    fn outer_qfile_with_spaces_in_path_is_found() {
        let a = engine_flags(&["x".into(), "cats math.CO".into(), "--qfile".into(), "/some dir/venues.tsv".into()]);
        assert_eq!(a.get("qfile"), Some("/some dir/venues.tsv"));
    }

    #[test]
    fn scripts_that_start_with_options_are_kept() {
        let rest: Vec<String> = ["--par=1 --cat cs.AI -n1 ; cats math.CO"].iter().map(|s| s.to_string()).collect();
        assert_eq!(script_text(&rest), "--par=1 --cat cs.AI -n1 ; cats math.CO");
        let a = engine_flags(&["x".into(), "--qfile=/some dir/q.tsv".into()]);
        assert_eq!(a.get("qfile"), Some("/some dir/q.tsv"));
        assert_eq!(a.all("qfile").len(), 1);
        let rest: Vec<String> = ["cats", "--qfile=/some dir/q.tsv"].iter().map(|s| s.to_string()).collect();
        assert_eq!(script_text(&rest), "cats");
    }

    #[test]
    fn engine_flags_inline_forms() {
        let a = engine_flags(&["get".into(), "1".into(), "--ttl=0".into(), "-j1".into(), "--qfile=/x".into()]);
        assert_eq!(a.num::<u64>("ttl"), Some(0));
        assert_eq!(a.num::<usize>("par"), Some(1));
        assert_eq!(a.get("qfile"), Some("/x"));
    }
    #[test]
    fn engine_flags_ignore_positionals_and_values() {
        let a = engine_flags(&["cats".into(), "--".into(), "--qfile=/nope".into()]);
        assert!(!a.flag("qfile"));
        // a value that merely looks like a flag belongs to its option
        let a = engine_flags(&["search".into(), "-a".into(), "--fresh".into()]);
        assert!(!a.flag("fresh"));
    }
    #[test]
    fn engine_flags_inside_x_after_outer_dashdash() {
        let a = engine_flags(&["x".into(), "--".into(), "cats --qfile=/q.tsv".into()]);
        assert_eq!(a.get("qfile"), Some("/q.tsv"));
    }
    #[test]
    fn engine_flags_in_nested_batches() {
        let a = engine_flags(&["x".into(), "x \"cats --qfile=/deep.tsv\"".into()]);
        assert_eq!(a.get("qfile"), Some("/deep.tsv"));
    }
    #[test]
    fn engine_flags_from_batch_string() {
        let a = engine_flags(&["x".to_string(), "search q --fresh --rate 6000 ; get 1 -j 2".to_string()]);
        assert!(a.flag("fresh"));
        assert_eq!(a.num::<u64>("rate"), Some(6000));
        assert_eq!(a.num::<usize>("par"), Some(2));
    }
}
