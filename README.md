# arx

A Rust arXiv CLI built for LLM agents that help scientific researchers. One binary, dense output, many actions per invocation, everything concurrent.

Design goals:

* **Few tokens in**: `arx -h` is ~500 tokens (no tool schemas to load, no JSON-RPC envelope). `arx help flags` prints every flag meaning when needed.
* **Few tokens out**: one line per paper (`id|date|authors|cat|title`), empty columns dropped, authors as initials, `--tok N` clips on whole records, `--facet` returns counts instead of rows.
* **Fast**: single static binary (mimalloc, LTO), hand-rolled Atom/HTML/TeX parsers, HTTP/2 with bounded concurrency, on-disk cache, request batching (`id_list`), page sizes tuned to the filters in use.
* **Polite**: API calls are gated across *all* arx processes to arXiv's 1 request / 3 s policy (`--rate`), with retry/backoff on 429/5xx.

## Install

```
cargo install --path .
```

## Tour

```
arx 'functional equation' -c math.CA --since 3y -n5            # search; no command word needed
arx -a 'terence tao' --cat number-theory --published --q 1,2   # fuzzy author, Q1/Q2 journals only
arx 'diffusion model' --preprint --inst 'mit,stanford' -n20     # preprints from author institutions
arx get 2301.01234 math/0309285 -f id,t,jr,co,ab               # metadata
arx prob 2411.00001 --kind conj,q                              # conjectures / open problems from full text
arx outline ID ; arx sec ID 2.1 ; arx grep ID 'sieve'          # read a paper piece by piece
arx refs ID ; arx cites ID ; arx related ID                     # bibliography, citing papers, similar papers
arx x 'search diffusion -n3 | prob ; get 2301.00001 ; cats comb'  # parallel statements, piped stages
arx -c math.CO --since 1y --facet year,pcat,q                  # counts instead of rows
arx watch graphs 'ramsey' -c math.CO ; arx alerts              # saved queries; alerts = new since last check
```

## Commands

| command | purpose |
|---|---|
| `search` (default) | query + all filters |
| `get`, `bib`, `com` | metadata, BibTeX, author comments |
| `prob` | theorem-like blocks (open problems, conjectures, theorems, ...) from full text, HTML first, TeX source fallback |
| `text`, `outline`, `sec`, `grep`, `refs` | full text as Markdown, sections, one section, passage search, bibliography |
| `cites`, `related`, `ver`, `dl` | citing papers (Semantic Scholar), similar papers, version history, downloads |
| `cats`, `tags`, `venue` | category taxonomy, topic presets, journal/conference quartiles |
| `watch`, `unwatch`, `watches`, `alerts` | saved queries with resumable cursors |
| `x` | batch: `;` runs statements concurrently, `\|` pipes the papers of one stage into the next |

## Filters

Server-side (fast): text/`--ti`/`--abs`/`--kw`/`--not`/`--co`/`--jr`, `--au`, `--cat`/`--xcat`/`--tag`, `--since`/`--until`/`--upd`, `--id`, `--sort`.

Client-side (over-fetching, concurrent enrichment): fuzzy author (accents, initials, particles, compound surnames), institution (`--inst`, from the full-text author block), `--published` / `--preprint` / `--accepted`, journal quartile `--q 1-2` (bundled, approximate SJR-style table plus CORE-mapped conferences; extend with `--qfile`), `--jn`, `--cmt`, `--pages`, `--nau`, `--msc`, `--pcat`, `--doi`.

Rules: comma inside a flag = OR, repeating a flag = AND. Dates: `2024`, `2024-03`, `2024-03-05`, `30d`, `6m`, `2y`.

## Notes and known limits

* Venue quartiles come from `data/journals.tsv` (approximate; unknown venues are left unclassified rather than guessed).
* Institution matching is word-order tolerant ("Oxford University" matches "University of Oxford"), so "University of Washington" also matches "Washington University".
* TeX theorem numbering follows `\newtheorem` counters (sections, shared theorem counters); numbers driven by other counters (`\setcounter`, equation-shared counters) are omitted rather than invented.
* A quoted `x` script that begins with `--qfile=PATH more words` is ambiguous with a qfile path containing spaces; pass such scripts after `--`.
* Caches live in `$ARX_CACHE` (default `~/.cache/arx`), watches in `$ARX_DATA` (default `~/.local/share/arx`).
