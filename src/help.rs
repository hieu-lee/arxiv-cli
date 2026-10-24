//! Help text. Flags section is generated from the FLAGS table so docs cannot drift from the parser.

use crate::args::FLAGS;

const HEAD: &str = "arx: arXiv for LLMs. `arx [CMD] ARGS FLAGS`; no CMD = search (a query starting with a command word needs `search`); ids alone = get.
Rules: comma = OR, repeated flag = AND. Dates: 2024 | 2024-03 | 30d | 6m | 2y. Output dense: id|date|authors|cat|title.
Batch: `arx x 'search diffusion -n3 | prob ; get 2301.00001'`  (`;` runs in parallel, `|` pipes papers to the next stage).
CMDS
 search Q        query+filter (default). Q: words, \"phrase\", AND OR NOT, ti:/au:/abs:/cat: prefixes
 get ID..        metadata+abstract      bib ID../Q  BibTeX      com ID../Q  author comments
 prob ID../Q     open problems/conjectures/theorems from full text (--kind)
 text ID         full text md (--sec, --tok)           outline ID  sections      sec ID KEY  one section
 grep ID PAT     passages (-g, --ctx)                  refs ID  bibliography       cites ID  citing papers (Semantic Scholar)
 related ID..     similar papers             dl ID.. [--src pdf|tex|html --to DIR]  download
 ver ID..        version history                       cats [F]  categories        tags  topic presets   venue [NAME]  journal quartile
 watch NAME Q / unwatch NAME / watches / alerts [NAME]   saved queries, alerts = new since last check
 cache [clear]   x STMTS  batch (stdin with -)         help [CMD]
";

/// Compact flag listing: names + metavars only, grouped (descriptions via `arx help flags`).
pub fn full() -> String {
    let mut s = String::from(HEAD);
    let mut group = "";
    for f in FLAGS {
        if f.group != group {
            group = f.group;
            s.push_str(&format!("\n{}:", group.to_ascii_uppercase()));
        }
        s.push(' ');
        if let Some(c) = f.short {
            s.push_str(&format!("-{c}|"));
        }
        s.push_str(&format!("--{}", f.name));
        if !f.arg.is_empty() {
            s.push(' ');
            s.push_str(f.arg);
        }
    }
    s.push_str("\nHelp: `arx help flags` (all flag meanings), `arx help CMD`.\n");
    s.push_str("EX arx x 'search -a \"terence tao\" -c math.NT --since 3y -n5 ; search sieve --published --q 1,2 | prob --kind conj,q'
");
    s
}

/// One line per flag with its meaning.
pub fn flags_full() -> String {
    let mut s = String::new();
    let mut group = "";
    for f in FLAGS {
        if f.group != group {
            group = f.group;
            s.push_str(&format!("{}\n", group.to_ascii_uppercase()));
        }
        s.push(' ');
        if let Some(c) = f.short {
            s.push_str(&format!("-{c}|"));
        }
        s.push_str(&format!("--{}", f.name));
        if !f.arg.is_empty() {
            s.push(' ');
            s.push_str(f.arg);
        }
        s.push_str("  ");
        s.push_str(f.help);
        s.push('\n');
    }
    s
}

pub fn topic(cmd: &str) -> String {
    let t = match cmd {
        "search" => "search: arx [search] [TEXT] [-a AUTHOR] [-c CAT] [-t TAG] [--since D] [-n N] [filters] [--facet cat,year]\n  e.g. arx -a 'erdos' --preprint --since 5y; arx 'functional equation' -c math.CA --q 1 --inst mit",
        "get" => "get: arx get ID.. [-f fields] [--fmt json|bib|tsv]  (ids may be URLs/arXiv:ids)",
        "prob" => "prob: arx prob ID..|QUERY [--kind prob,conj,q,thm,lem,def,all] [-m N] [-g PAT] [--src tex]  HTML (ar5iv for older papers); --src tex fetches the e-print source on request",
        "text" => "text: arx text ID [--sec 2.1|title] [--tok N] [--skip CHARS] [--src tex]",
        "watch" => "watch: arx watch NAME <search args>; arx alerts [NAME] [--peek]",
        "x" => "x: arx x 'STMT ; STMT | STAGE'  parallel statements, piped stages (--keep prints a middle stage). `-` reads stdin",
        "flags" => return flags_full(),
        _ => return full(),
    };
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_help_lists_every_flag_and_stays_small() {
        let h = full();
        for f in FLAGS {
            assert!(h.contains(&format!("--{}", f.name)), "{}", f.name);
        }
        assert!(h.len() < 2500, "compact help grew to {} bytes", h.len());
        assert_eq!(flags_full().lines().filter(|l| l.starts_with(" ")).count(), FLAGS.len());
    }
}
