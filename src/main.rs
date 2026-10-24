mod args;
mod atom;
mod cmd;
mod dates;
mod engine;
mod fallback;
mod filter;
mod fuzzy;
mod help;
mod html;
mod model;
mod net;
mod openalex;
mod out;
mod pipe;
mod pstore;
mod query;
mod store;
mod tags;
mod taxonomy;
mod tex;
mod venue;

#[global_allocator]
static A: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // the regex matcher recurses deeply; run the whole CLI on a large stack (worker threads get the same)
    let h = std::thread::Builder::new().stack_size(64 << 20).spawn(real_main).expect("spawn main thread");
    let code = h.join().unwrap_or(2);
    std::process::exit(code);
}

/// One quoted argument that is really a whole command line ("search foo -n 3"): it starts with a
/// canonical command name AND carries a flag. Plain quoted queries ("cat state", "text mining") stay queries.
fn looks_like_command_line(s: &str) -> bool {
    let mut words = s.split_whitespace();
    let first = words.next().unwrap_or("");
    pipe::canonical(first) == Some(first) && words.any(|w| w.starts_with('-') && w.len() > 1)
}

fn real_main() -> i32 {
    use std::io::Write;
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.len() == 1 && argv[0].contains(char::is_whitespace) && (argv[0].contains([';', '|', '\n']) || looks_like_command_line(&argv[0])) {
        match args::tokenize(&argv[0]) {
            Ok(t) => argv = t,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        }
    }
    // `arx x -` reads its statements from stdin; expand before engine flags are collected
    if argv.first().and_then(|w| pipe::canonical(w)) == Some("x") && argv.iter().filter(|t| *t == "-").count() > 1 {
        eprintln!("error: `-` (stdin) may appear only once");
        return 2;
    }
    if argv.first().and_then(|w| pipe::canonical(w)) == Some("x") && argv.iter().any(|t| t == "-") {
        let input = match pipe::read_stdin() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        for t in argv.iter_mut().skip(1) {
            if t == "-" {
                *t = input.clone();
            }
        }
    }
    if argv.is_empty() || matches!(argv.as_slice(), [h] if h == "-h" || h == "--help") {
        return match write!(std::io::stdout().lock(), "{}", help::full()) {
            Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
                eprintln!("error: write failed: {e}");
                1
            }
            _ => 0,
        };
    }
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(64 << 20).build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: runtime: {e}");
            std::process::exit(2);
        }
    };
    let code = rt.block_on(async {
        // Flags are parsed per stage; global engine flags are read from the first stage's args.
        let probe = pipe::engine_flags(&argv);
        if let Err(e) = pipe::load_qfiles(&probe) {
            eprintln!("error: {e}");
            return 2;
        }
        let net = match net::Net::new(&probe) {
            Ok(n) => std::sync::Arc::new(n),
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        let (text, ok, cursors) = pipe::run_batch(&net, &argv).await;
        // where the answer came from when it was degraded (stale cache, OpenAlex, cooling hosts)
        for n in net.take_notes() {
            let _ = writeln!(std::io::stderr().lock(), "(note: {n})");
        }
        let to_err = text.starts_with("error: ");
        let written = if to_err { writeln!(std::io::stderr().lock(), "{text}") } else { writeln!(std::io::stdout().lock(), "{text}") };
        if let Err(e) = written {
            // a closed pipe (`arx ... | head`) is normal truncation; anything else (disk full) is a failure
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                eprintln!("error: write failed: {e}");
                return 1;
            }
        } else if let Err(e) = pipe::commit_cursors(cursors) {
            eprintln!("error: {e}");
            return 1;
        }
        if ok {
            0
        } else {
            1
        }
    });
    code
}
