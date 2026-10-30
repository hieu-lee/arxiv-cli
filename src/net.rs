//! HTTP engine: shared client, cross-process API rate gate, on-disk cache, retry/backoff.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::args::Args;

pub const API: &str = "https://export.arxiv.org/api/query";
const UA: &str = concat!("arx/", env!("CARGO_PKG_VERSION"), " (LLM-first arXiv CLI; +https://github.com/hieu-lee/arxiv-cli)");
const TTL_API: u64 = 3600;
const TTL_DOC: u64 = 7 * 86400;
const MAX_TRIES: u32 = 2;
const WWW_GAP_MS: u64 = 400;
/// arXiv API etiquette: at most one request every three seconds, one connection at a time.
const API_GAP_MS: u64 = 3000;

#[derive(Clone, Copy)]
pub enum Ttl {
    Api,
    Doc,
}

/// Where data may come from (`--via`): `auto` = cache, arXiv, then fallbacks; `api` = arXiv only;
/// `oa` = OpenAlex only; `cache` = never touch the network (stale entries allowed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Via {
    Auto,
    Api,
    Oa,
    Cache,
}

impl Via {
    pub fn parse(s: &str) -> Result<Via, String> {
        match s {
            "auto" => Ok(Via::Auto),
            "api" | "arxiv" => Ok(Via::Api),
            "oa" | "openalex" => Ok(Via::Oa),
            "cache" | "offline" => Ok(Via::Cache),
            o => Err(format!("bad --via {o:?} (auto|api|oa|cache)")),
        }
    }
}

/// Prefix of every error that means "the network/host was unavailable" (as opposed to a bad query),
/// which is what makes fallbacks (stale cache, OpenAlex) appropriate.
pub const NET_ERR: &str = "[net]";

pub fn is_net_err(e: &str) -> bool {
    e.starts_with(NET_ERR)
}

/// Held for the duration of one API request (see `Net::gate`).
struct ApiGuard<'a> {
    _local: Option<tokio::sync::MutexGuard<'a, Option<std::time::Instant>>>,
    _file: Option<fs::File>,
}

pub struct Net {
    /// Built on first fetch: TLS setup is wasted work for runs served from cache or without network.
    http: std::sync::OnceLock<reqwest::Client>,
    sem: Semaphore,
    /// In-process spacing (always enforced, even when the cross-process stamp file is unusable).
    local_gate: tokio::sync::Mutex<Option<std::time::Instant>>,
    /// Start spacing for arxiv.org page fetches (the site asks automated clients for a crawl delay).
    www_gate: tokio::sync::Mutex<Option<std::time::Instant>>,
    gap_ms: u64,
    ttl_api: u64,
    ttl_doc: u64,
    ttl_user: Option<u64>,
    fresh: bool,
    dir: Option<PathBuf>,
    pub via: Via,
    notes: std::sync::Mutex<Vec<String>>,
    /// Set once any answer came from stale or substitute data (alerts must not advance cursors then).
    degraded: std::sync::atomic::AtomicBool,
    /// Cooldown deadlines (ms) kept in memory too, so the breaker still works when the cache dir is unwritable.
    cool_tx: std::sync::Mutex<()>, // serialises cooldown read-modify-write inside this process
    mem_cool: std::sync::Mutex<std::collections::HashMap<String, (u128, u32, u128)>>, // full record: survives an unwritable cache dir
    /// OpenAlex credentials (`ARX_OPENALEX_KEY`, `ARX_MAILTO`).
    pub oa_key: Option<String>,
    pub mailto: Option<String>,
}

pub fn cache_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ARX_CACHE") {
        return Some(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(d).join("arx"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("arx"))
}

pub fn data_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ARX_DATA") {
        return Some(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(d).join("arx"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share").join("arx"))
}

/// `DIR/<16 hex digits of h>` (plus `.tmp<pid>` for the staging file), built with a single allocation.
fn entry_path(dir: &std::path::Path, h: u64, tmp_pid: Option<u32>) -> PathBuf {
    let mut name = [0u8; 40];
    for (i, d) in name[..16].iter_mut().enumerate() {
        *d = b"0123456789abcdef"[(h >> (60 - 4 * i) & 0xf) as usize];
    }
    let mut len = 16;
    if let Some(pid) = tmp_pid {
        name[16..20].copy_from_slice(b".tmp");
        let left = {
            let mut rest = &mut name[20..];
            let _ = write!(rest, "{pid}");
            rest.len()
        };
        len = 40 - left;
    }
    let mut p = PathBuf::with_capacity(dir.as_os_str().len() + 1 + len);
    p.push(dir);
    p.push(std::str::from_utf8(&name[..len]).unwrap_or_default());
    p
}

/// `write_all` of the concatenated `parts` with as few syscalls as the OS allows.
fn write_all_parts(f: &mut fs::File, parts: &[&[u8]; 3]) -> std::io::Result<()> {
    let slices = parts.map(std::io::IoSlice::new);
    let mut skip = f.write_vectored(&slices)?;
    for p in parts {
        if skip >= p.len() {
            skip -= p.len();
        } else {
            f.write_all(&p[skip..])?;
            skip = 0;
        }
    }
    Ok(())
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Percent-encode for a query-string value (space -> '+').
pub fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'*' | b':' => o.push(b as char),
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

pub fn api_url(params: &[(&str, String)]) -> String {
    let mut u = String::from(API);
    for (i, (k, v)) in params.iter().enumerate() {
        u.push(if i == 0 { '?' } else { '&' });
        u.push_str(k);
        u.push('=');
        u.push_str(&enc(v));
    }
    u
}

/// Cache namespace key of a host: the arXiv API and the arXiv website throttle independently.
pub fn host_key(url: &str) -> String {
    if url.starts_with(API) {
        return "api".into();
    }
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let host = rest.split(['/', '?']).next().unwrap_or(rest);
    if host == "arxiv.org" || host.ends_with(".arxiv.org") {
        "www".into()
    } else {
        host.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-').collect()
    }
}

/// `Retry-After`: delta-seconds or an HTTP-date ("Sun, 06 Nov 1994 08:49:37 GMT"), as seconds from `now_ms`.
pub fn parse_retry_after(v: &str, now_ms: u128) -> Option<u64> {
    let v = v.trim();
    if let Ok(s) = v.parse::<u64>() {
        return Some(s);
    }
    let rest = v.split_once(',').map_or(v, |x| x.1).trim();
    let mut it = rest.split_whitespace();
    let day: u32 = it.next()?.parse().ok()?;
    let mon = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter()
        .position(|m| it.clone().next().is_some_and(|x| x.to_ascii_lowercase().starts_with(m)))? as u32
        + 1;
    it.next();
    let year: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, sec) = (hms.next()??, hms.next()??, hms.next()??);
    if !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    let epoch = crate::dates::days_from_civil(year, mon, day) as i128 * 86_400 + (h * 3600 + m * 60 + sec) as i128;
    let delta = epoch - (now_ms / 1000) as i128;
    Some(delta.max(0) as u64)
}

/// The URL without credentials (`api_key=`): used for cache identity and every message.
pub fn strip_secrets(url: &str) -> String {
    let Some((base, q)) = url.split_once('?') else { return url.to_string() };
    let kept: Vec<&str> = q.split('&').filter(|p| !p.starts_with("api_key=") && !p.starts_with("mailto=")).collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

const COOL_CAP_SECS: u64 = 900;
const RETRY_AFTER_CAP_SECS: u64 = 86_400;
const COOL_MAX_LEVEL: u32 = 6;

impl Net {
    pub fn new(a: &Args) -> Result<Net, String> {
        let par = a.num::<usize>("par").unwrap_or(4).clamp(1, 64);
        let via = match a.get("via") {
            Some(v) => Via::parse(v)?,
            None => Via::Auto,
        };
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Ok(Net {
            http: std::sync::OnceLock::new(),
            sem: Semaphore::new(par),
            local_gate: tokio::sync::Mutex::new(None),
            www_gate: tokio::sync::Mutex::new(None),
            // arXiv's policy is one request per 3 s: a smaller --rate is raised to that floor
            gap_ms: a.num::<u64>("rate").unwrap_or(API_GAP_MS).max(API_GAP_MS),
            ttl_api: a.num::<u64>("ttl").unwrap_or(TTL_API),
            ttl_doc: a.num::<u64>("ttl").unwrap_or(TTL_DOC),
            ttl_user: a.num::<u64>("ttl"),
            fresh: a.flag("fresh"),
            dir: cache_dir(),
            via,
            notes: std::sync::Mutex::new(Vec::new()),
            degraded: std::sync::atomic::AtomicBool::new(false),
            cool_tx: std::sync::Mutex::new(()),
            mem_cool: std::sync::Mutex::new(std::collections::HashMap::new()),
            oa_key: env("ARX_OPENALEX_KEY"),
            mailto: env("ARX_MAILTO"),
        })
    }

    fn client(&self) -> Result<&reqwest::Client, String> {
        if let Some(c) = self.http.get() {
            return Ok(c);
        }
        let c = reqwest::Client::builder()
            .user_agent(UA)
            .gzip(true)
            .brotli(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(self.http.get_or_init(|| c))
    }

    /// Remember a caveat about where the answer came from (printed once, on stderr, by main).
    pub fn note(&self, msg: impl Into<String>) {
        let msg = msg.into();
        let mut n = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        if !n.contains(&msg) {
            n.push(msg);
        }
    }

    /// Like `note`, for answers that may be incomplete or out of date (stale cache, other sources).
    pub fn degrade(&self, msg: impl Into<String>) {
        self.degraded.store(true, std::sync::atomic::Ordering::Relaxed);
        self.note(msg);
    }

    pub fn degraded(&self) -> bool {
        self.degraded.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Test constructor: a Net rooted in `dir` (no env lookups).
    #[cfg(test)]
    pub(crate) fn for_test(dir: &std::path::Path, extra: &[&str]) -> Net {
        let toks: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        let mut n = Net::new(&crate::args::parse(&toks).unwrap()).unwrap();
        n.dir = Some(dir.to_path_buf());
        n
    }

    /// Freshness window for the per-paper store: its 24 h default, shortened by an explicit `--ttl`.
    pub(crate) fn store_ttl(&self, default: u64) -> u64 {
        self.ttl_user.map_or(default, |t| t.min(default))
    }

    #[cfg(test)]
    pub(crate) fn cache_file(&self, url: &str) -> Option<PathBuf> {
        self.cache_path(url)
    }

    fn cache_path(&self, url: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| entry_path(d, fnv(url), None))
    }

    /// Ok(Some(Some(bytes))) hit, Ok(Some(None)) cached 404, None miss. `max_age`: None = any age (stale
    /// allowed); `honor_fresh`: `--fresh` turns every read into a miss.
    pub(crate) fn cache_read(&self, url: &str, max_age: Option<u64>, honor_fresh: bool) -> Option<Option<Vec<u8>>> {
        if honor_fresh && self.fresh {
            return None;
        }
        let p = self.cache_path(url)?;
        if url.contains('\n') {
            return None;
        }
        let mut file = fs::File::open(&p).ok()?;
        let meta = file.metadata().ok()?;
        if let Some(ttl) = max_age {
            if meta.modified().ok()?.elapsed().ok()?.as_secs() > ttl {
                return None;
            }
        }
        // the entry is `URL\nBODY`: check the key, then read the body straight into its own buffer
        let mut buf = Vec::with_capacity(meta.len() as usize);
        buf.resize(url.len() + 1, 0);
        file.read_exact(&mut buf).ok()?;
        if buf[..url.len()] != *url.as_bytes() || buf[url.len()] != b'\n' {
            return None;
        }
        buf.clear();
        file.read_to_end(&mut buf).ok()?;
        if buf == b"\0404" {
            return Some(None);
        }
        Some(Some(buf))
    }

    pub(crate) fn cache_put(&self, url: &str, body: Option<&[u8]>) {
        let Some(dir) = self.dir.as_deref() else { return };
        let h = fnv(url);
        let (p, tmp) = (entry_path(dir, h, None), entry_path(dir, h, Some(std::process::id())));
        let write = || -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            write_all_parts(&mut f, &[url.as_bytes(), b"\n", body.unwrap_or(b"\0404")])
        };
        let mut ok = write();
        if ok.is_err() {
            // first write into a fresh cache dir
            if let Some(d) = p.parent() {
                let _ = fs::create_dir_all(d);
            }
            ok = write();
        }
        if ok.is_ok() {
            let _ = fs::rename(&tmp, &p);
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }

    // ------------------------------------------------------------ throttle circuit breaker

    fn cool_path(&self, key: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("cool-{key}")))
    }

    /// Seconds left of a cooldown recorded (by any arx process) for this host group.
    pub fn cooldown_left(&self, key: &str) -> Option<u64> {
        let now = crate::dates::unix_ms();
        let until = self.read_cool(key).0;
        (until > now).then(|| ((until - now) / 1000 + 1) as u64)
    }

    /// Run `f` holding a named cross-process lock in the cache dir (no-op protection if it is unusable).
    pub(crate) fn with_lock<R>(&self, name: &str, f: impl FnOnce() -> R) -> R {
        let lock = self.dir.as_ref().and_then(|d| {
            fs::create_dir_all(d).ok()?;
            let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(d.join(format!("{name}.lock"))).ok()?;
            f.lock().ok()?;
            Some(f)
        });
        let r = f();
        drop(lock);
        r
    }

    /// Run `f` under the per-host cooldown lock (cross-process), so concurrent trips/clears cannot lose updates.
    fn with_cool_lock<R>(&self, key: &str, f: impl FnOnce() -> R) -> R {
        let _tx = self.cool_tx.lock().unwrap_or_else(|e| e.into_inner());
        self.with_lock(&format!("cool-{key}"), f)
    }

    fn mem_record(&self, key: &str) -> Option<(u128, u32, u128)> {
        self.mem_cool.lock().unwrap_or_else(|e| e.into_inner()).get(key).copied()
    }

    /// Cooldown record: (until_ms, level, last_trip_ms), merging the file with this process's memory
    /// (the file may be unwritable): latest deadline, level history of the latest trip.
    fn read_cool(&self, key: &str) -> (u128, u32, u128) {
        let file = self.read_cool_file(key);
        match self.mem_record(key) {
            Some(m) if m.2 > file.2 => (m.0.max(file.0), m.1, m.2),
            Some(m) => (m.0.max(file.0), file.1, file.2),
            None => file,
        }
    }

    fn read_cool_file(&self, key: &str) -> (u128, u32, u128) {
        let s = self.cool_path(key).and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
        let mut it = s.split_whitespace();
        let until = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let level = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let last = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        (until, level, last)
    }

    fn write_cool(&self, key: &str, rec: (u128, u32, u128)) {
        self.mem_cool.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), rec);
        if let Some(p) = self.cool_path(key) {
            let tmp = p.with_extension(format!("tmp{}", std::process::id()));
            if fs::write(&tmp, format!("{} {} {}", rec.0, rec.1, rec.2)).is_ok() {
                let _ = fs::rename(&tmp, &p);
            }
        }
    }

    /// Record a throttle/outage: back off exponentially (30 s .. 15 min for HTTP 429, 10 s based for
    /// transient failures); a server-given `Retry-After` is obeyed as given (up to a day). An active
    /// cooldown is never shortened. Shared by all arx processes through the cache dir. Returns the
    /// seconds the host is now off limits.
    fn trip(&self, key: &str, retry_after: Option<u64>, transient: bool) -> u64 {
        self.with_cool_lock(key, || {
            let now = crate::dates::unix_ms();
            let (prev_until, level, last) = self.read_cool(key);
            let level = if now.saturating_sub(last) > 3_600_000 { 1 } else { (level + 1).min(COOL_MAX_LEVEL) };
            let base: u64 = if transient { 10 } else { 30 };
            let secs = match retry_after {
                Some(s) => s.clamp(1, RETRY_AFTER_CAP_SECS),
                None => (base << (level - 1)).min(COOL_CAP_SECS),
            };
            let until = prev_until.max(now + secs as u128 * 1000);
            self.write_cool(key, (until, level, now));
            ((until - now) / 1000 + 1) as u64
        })
    }

    /// A success ends the cooldown, unless a (concurrent) request tripped it after this one started.
    fn clear_cooldown(&self, key: &str, started_ms: u128) {
        if !self.cool_path(key).is_some_and(|p| p.exists()) {
            return;
        }
        self.with_cool_lock(key, || {
            let (until, level, last) = self.read_cool(key);
            // read_cool merges memory (a trip whose file write failed) with the file: `last` is the latest trip
            if until != 0 && last < started_ms {
                self.write_cool(key, (0, level, last)); // level history decays after an hour
            }
        });
    }

    // ------------------------------------------------------------ cross-process API gate

    /// Cross-process politeness gate: one API connection at a time across all arx processes, at least
    /// `gap_ms` between starts. The returned guard must be held until the response body is consumed.
    async fn gate(&self) -> ApiGuard<'_> {
        let mut last = self.local_gate.lock().await;
        let gap_d = Duration::from_millis(self.gap_ms);
        if let Some(t) = *last {
            let el = t.elapsed();
            // a queued caller must not sleep through a cooldown another request just opened
            if el < gap_d && self.cooldown_left("api").is_none() {
                tokio::time::sleep(gap_d - el).await;
            }
        }
        let file = match self.dir.clone() {
            Some(dir) => self.gate_across_processes(dir).await,
            None => None,
        };
        *last = Some(std::time::Instant::now());
        ApiGuard { _local: Some(last), _file: file }
    }

    /// Takes the cross-process file lock (kept until the returned file is dropped) after waiting out the gap.
    async fn gate_across_processes(&self, dir: PathBuf) -> Option<fs::File> {
        let gap = self.gap_ms as u128;
        tokio::task::spawn_blocking(move || -> std::io::Result<fs::File> {
            fs::create_dir_all(&dir)?;
            let mut f = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("api.stamp"))?;
            f.lock()?;
            let now = crate::dates::unix_ms();
            // cooling down (opened by whoever held the lock before us): no request will follow, so neither
            // wait out the gap nor advance the stamp; the caller sees the cooldown and returns at once
            let until: u128 = fs::read_to_string(dir.join("cool-api")).ok().and_then(|c| c.split_whitespace().next()?.parse().ok()).unwrap_or(0);
            if until > now {
                return Ok(f);
            }
            let mut s = String::new();
            f.read_to_string(&mut s)?;
            let last: u128 = s.trim().parse().unwrap_or(0);
            if last + gap > now {
                std::thread::sleep(Duration::from_millis((last + gap - now) as u64));
            }
            f.seek(SeekFrom::Start(0))?;
            f.set_len(0)?;
            write!(f, "{}", crate::dates::unix_ms())?;
            Ok(f) // still locked: other processes wait until this request has finished
        })
        .await
        .ok()
        .and_then(Result::ok)
    }

    // ------------------------------------------------------------ fetching

    /// One network fetch with fail-fast throttling: HTTP 429 (or an outage) opens a shared cooldown and
    /// returns immediately instead of retry-hammering; callers then use stale cache or another source.
    async fn fetch(&self, url: &str, key: &str) -> Result<Option<Vec<u8>>, String> {
        let cooling = |s: u64| format!("{NET_ERR} {key} is cooling down for {s}s after throttling; using cached/fallback data where possible");
        if let Some(s) = self.cooldown_left(key) {
            return Err(cooling(s));
        }
        let shown = strip_secrets(url);
        let api = key == "api";
        let started = crate::dates::unix_ms();
        let http = self.client()?;
        let mut last_err = String::new();
        for attempt in 0..MAX_TRIES {
            let gate = if api { Some(self.gate().await) } else { None };
            let _permit = if api { None } else { Some(self.sem.acquire().await.map_err(|e| e.to_string())?) };
            if key == "www" {
                // spread arxiv.org page fetches out (default 400 ms between starts) instead of bursting
                let mut last = self.www_gate.lock().await;
                let gap = Duration::from_millis(WWW_GAP_MS);
                if let Some(t) = *last {
                    tokio::time::sleep(gap.saturating_sub(t.elapsed())).await;
                }
                *last = Some(std::time::Instant::now());
            }
            // the gate wait may have outlasted a cooldown another process just opened
            if let Some(s) = self.cooldown_left(key) {
                return Err(cooling(s));
            }
            let retry = attempt + 1 < MAX_TRIES;
            match http.get(url).timeout(Duration::from_secs(if api { 30 } else { 60 })).send().await {
                Ok(r) => {
                    let st = r.status();
                    let retry_after = r
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| parse_retry_after(v, crate::dates::unix_ms()));
                    if st.is_success() {
                        match r.bytes().await {
                            Ok(b) => {
                                self.clear_cooldown(key, started);
                                return Ok(Some(b.to_vec()));
                            }
                            Err(e) if e.is_timeout() => {
                                // headers arrived but the body stalled: the same hanging-host signal as a connect timeout
                                let secs = self.trip(key, None, true);
                                return Err(format!("{NET_ERR} {shown}: body timed out; {key} cooling down {secs}s"));
                            }
                            Err(e) => last_err = format!("{NET_ERR} {shown}: {}", e.without_url()),
                        }
                    } else if st.as_u16() == 404 {
                        return Ok(None);
                    } else if st.as_u16() == 429 || (st.as_u16() == 403 && key == "www") || (st.is_server_error() && retry_after.is_some()) {
                        let secs = self.trip(key, retry_after, false);
                        return Err(format!("{NET_ERR} {shown}: HTTP {st}; {key} throttled, cooling down {secs}s"));
                    } else if st.is_server_error() {
                        last_err = format!("{NET_ERR} {shown}: HTTP {st}");
                    } else {
                        // a rejected request (400, 422, ...) is the caller's problem, not an outage: no fallbacks
                        return Err(format!("{shown}: HTTP {st}"));
                    }
                }
                Err(e) if e.is_timeout() => {
                    // a hanging host is the typical shape of throttling: stop waiting, cool down
                    let secs = self.trip(key, None, true);
                    return Err(format!("{NET_ERR} {shown}: timed out; {key} cooling down {secs}s"));
                }
                Err(e) => last_err = format!("{NET_ERR} {shown}: {}", e.without_url()),
            }
            drop(_permit);
            drop(gate);
            if retry {
                tokio::time::sleep(Duration::from_millis(600)).await;
            }
        }
        self.trip(key, None, true);
        Err(last_err)
    }

    /// GET with cache. `None` = 404. Order: fresh cache, network, stale cache (outage/throttle).
    /// Returns whether the body came from an expired cache entry.
    pub async fn get_bytes_ex(&self, url: &str, ttl: Ttl) -> Result<(Option<Vec<u8>>, bool), String> {
        let key = host_key(url);
        // cache identity never contains credentials (and is shared between users of different keys)
        let ckey = strip_secrets(url);
        let secs = match ttl {
            Ttl::Api => self.ttl_api,
            Ttl::Doc => self.ttl_doc,
        };
        if self.via == Via::Cache {
            if let Some(hit) = self.cache_read(&ckey, Some(secs), true) {
                return Ok((hit, false));
            }
            return match self.cache_read(&ckey, None, false) {
                Some(stale) => {
                    self.degrade(format!("served stale cached data for {key} (--via cache)"));
                    Ok((stale, true))
                }
                None => Err(format!("{NET_ERR} not cached (--via cache): {ckey}")),
            };
        }
        if let Some(hit) = self.cache_read(&ckey, Some(secs), true) {
            return Ok((hit, false));
        }
        // the selected source is enforced here, at the one place every request passes through
        let openalex = key.contains("openalex");
        let arxiv_host = key == "api" || key == "www";
        if (self.via == Via::Oa && !openalex) || (self.via == Via::Api && !arxiv_host) {
            let only = if self.via == Via::Oa { "OpenAlex" } else { "arXiv" };
            return Err(format!("--via {}: {key} is not contacted (only {only} is allowed; cached copies are still used)", if self.via == Via::Oa { "oa" } else { "api" }));
        }
        match self.fetch(url, &key).await {
            Ok(r) => {
                self.cache_put(&ckey, r.as_deref());
                Ok((r, false))
            }
            // only outages/throttling fall back to stale data; a rejected request must surface
            Err(e) if is_net_err(&e) => match self.cache_read(&ckey, None, false) {
                Some(stale) => {
                    self.degrade(format!("served stale cached data for {key} ({})", e.trim_start_matches(NET_ERR).trim().chars().take(90).collect::<String>()));
                    Ok((stale, true))
                }
                None => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    pub async fn get_bytes(&self, url: &str, ttl: Ttl) -> Result<Option<Vec<u8>>, String> {
        Ok(self.get_bytes_ex(url, ttl).await?.0)
    }

    pub async fn get_text(&self, url: &str, ttl: Ttl) -> Result<Option<String>, String> {
        Ok(self.get_bytes(url, ttl).await?.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// arXiv API call; the flag tells whether the answer is an expired cache entry (do not treat it as fresh).
    pub async fn api_ex(&self, params: &[(&str, String)]) -> Result<(String, bool), String> {
        let (body, stale) = self.get_bytes_ex(&api_url(params), Ttl::Api).await?;
        let body = body.ok_or_else(|| "arXiv API returned 404".to_string())?;
        Ok((String::from_utf8_lossy(&body).into_owned(), stale))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net_in(dir: &std::path::Path, extra: &[&str]) -> Net {
        let toks: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        let mut n = Net::new(&crate::args::parse(&toks).unwrap()).unwrap();
        n.dir = Some(dir.to_path_buf());
        n
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arx-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn entry_names_are_16_hex_digits_with_a_pid_suffix_when_staged() {
        let d = std::path::Path::new("/c");
        assert_eq!(entry_path(d, 0xabc, None), d.join("0000000000000abc"));
        assert_eq!(entry_path(d, u64::MAX, Some(4242)), d.join("ffffffffffffffff.tmp4242"));
        assert_eq!(entry_path(d, 1, Some(u32::MAX)), d.join("0000000000000001.tmp4294967295"));
    }

    #[test]
    fn encodes() {
        assert_eq!(enc("ti:\"a b\" AND (x)"), "ti:%22a+b%22+AND+%28x%29");
        assert_eq!(enc("cat:math.*"), "cat:math.*");
        assert_eq!(enc("é"), "%C3%A9");
    }

    #[test]
    fn url_build() {
        let u = api_url(&[("search_query", "au:tao".into()), ("max_results", "5".into())]);
        assert_eq!(u, "https://export.arxiv.org/api/query?search_query=au:tao&max_results=5");
    }

    #[test]
    fn hosts_throttle_independently() {
        assert_eq!(host_key("https://export.arxiv.org/api/query?x=1"), "api");
        assert_eq!(host_key("https://arxiv.org/html/2301.00001"), "www");
        assert_eq!(host_key("https://ar5iv.labs.arxiv.org/html/1706.03762"), "www");
        assert_eq!(host_key("https://api.openalex.org/works?x"), "api.openalex.org");
    }

    #[test]
    fn cooldown_survives_an_unwritable_cache_dir() {
        let mut n = net_in(&tmp("nowrite"), &[]);
        n.dir = Some(PathBuf::from("/proc/arx-no-such-dir/cache"));
        n.trip("api", Some(60), false);
        assert!(n.cooldown_left("api").is_some(), "in-memory deadline keeps the breaker closed");
        let n = net_in(&tmp("nowrite2"), &[]);
        let mut n = n;
        n.dir = Some(PathBuf::from("/proc/arx-no-such-dir/cache"));
        let first = n.trip("www", None, false);
        assert!((30..=31).contains(&first), "{first}");
        assert!((60..=61).contains(&n.trip("www", None, false)), "memory keeps the escalation level");
    }

    #[test]
    fn older_success_keeps_an_unpersisted_newer_trip() {
        let d = tmp("memtrip");
        let n = net_in(&d, &[]);
        n.trip("api", Some(1), false);
        let started = crate::dates::unix_ms() + 5; // an in-flight request, older than the next trip
        let far = started + 3_600_000;
        n.mem_cool.lock().unwrap().insert("api".into(), (far, 1, started + 5)); // a later trip whose file write failed
        n.clear_cooldown("api", started);
        assert!(n.cooldown_left("api").is_some_and(|s| s > 3000));
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn queued_callers_do_not_wait_out_the_gap_during_a_cooldown() {
        let d = tmp("gatecool");
        let n = net_in(&d, &[]);
        let stamp = crate::dates::unix_ms().to_string();
        fs::write(d.join("api.stamp"), &stamp).unwrap();
        n.trip("api", Some(3600), false);
        let t = std::time::Instant::now();
        drop(n.gate().await);
        drop(n.gate().await);
        assert!(t.elapsed() < Duration::from_millis(1500), "{:?}", t.elapsed());
        assert_eq!(fs::read_to_string(d.join("api.stamp")).unwrap(), stamp, "no request, no stamp update");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn concurrent_memory_only_trips_keep_the_longest_deadline_and_escalate() {
        let mut n = net_in(&tmp("memrace"), &[]);
        n.dir = Some(PathBuf::from("/proc/arx-no-such-dir/cache"));
        let n = std::sync::Arc::new(n);
        let hs: Vec<_> = [Some(3600), None, None, None]
            .into_iter()
            .map(|ra| {
                let n = n.clone();
                std::thread::spawn(move || n.trip("www", ra, false))
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert!(n.cooldown_left("www").is_some_and(|s| s > 3500), "the one-hour deadline must survive");
        assert_eq!(n.read_cool("www").1, 4, "every trip advanced the level");
    }

    #[test]
    fn retry_after_formats() {
        assert_eq!(parse_retry_after("120", 0), Some(120));
        // 06 Nov 1994 08:49:37 GMT = 784111777
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_700_000), Some(77));
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", 784_200_000_000), Some(0));
        assert_eq!(parse_retry_after("soon", 0), None);
        assert_eq!(parse_retry_after("", 0), None);
    }

    #[test]
    fn credentials_never_reach_cache_identity_or_messages() {
        assert_eq!(strip_secrets("https://api.openalex.org/works?filter=x&api_key=SECRET&mailto=a@b.c&per_page=5"), "https://api.openalex.org/works?filter=x&per_page=5");
        assert_eq!(strip_secrets("https://api.openalex.org/works?api_key=S"), "https://api.openalex.org/works");
        assert_eq!(strip_secrets("https://x/y"), "https://x/y");
    }

    #[test]
    fn via_parses() {
        assert_eq!(Via::parse("oa"), Ok(Via::Oa));
        assert_eq!(Via::parse("offline"), Ok(Via::Cache));
        assert!(Via::parse("zzz").is_err());
    }

    #[test]
    fn cooldown_escalates_honours_retry_after_and_clears() {
        let d = tmp("cool");
        let n = net_in(&d, &[]);
        assert_eq!(n.cooldown_left("api"), None);
        let t0 = crate::dates::unix_ms();
        std::thread::sleep(Duration::from_millis(5)); // trips must be strictly after t0
        let first = n.trip("api", None, false);
        assert!((30..=31).contains(&first), "{first}");
        assert!(n.cooldown_left("api").is_some_and(|s| (1..=31).contains(&s)));
        assert!((60..=61).contains(&n.trip("api", None, false)));
        let third = n.trip("api", None, false);
        assert!((120..=121).contains(&third), "{third}");
        // a shorter Retry-After never shortens an active cooldown; a longer one extends it, up to a day
        assert!(n.trip("api", Some(7), false) >= 119);
        assert_eq!(n.trip("api", Some(100_000), false), RETRY_AFTER_CAP_SECS + 1);
        // an older in-flight success must not erase a newer trip
        n.clear_cooldown("api", t0);
        assert!(n.cooldown_left("api").is_some());
        // same-millisecond trip (started == last) is also kept
        let (_, _, last) = n.read_cool("api");
        n.clear_cooldown("api", last);
        assert!(n.cooldown_left("api").is_some());
        n.clear_cooldown("api", crate::dates::unix_ms() + 1);
        assert_eq!(n.cooldown_left("api"), None);
        // other hosts are unaffected
        n.trip("www", None, true);
        assert_eq!(n.cooldown_left("api"), None);
        assert!(n.cooldown_left("www").is_some());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_serves_stale_only_when_asked() {
        let d = tmp("stale");
        let n = net_in(&d, &[]);
        n.cache_put("u1", Some(b"body"));
        assert_eq!(n.cache_read("u1", Some(3600), true), Some(Some(b"body".to_vec())));
        n.cache_put("u404", None);
        assert_eq!(n.cache_read("u404", None, false), Some(None));
        assert_eq!(n.cache_read("missing", None, false), None);
        let f = net_in(&d, &["--fresh"]);
        assert_eq!(f.cache_read("u1", Some(3600), true), None, "--fresh skips fresh reads");
        assert_eq!(f.cache_read("u1", None, false), Some(Some(b"body".to_vec())), "but stale fallback still reads");
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn via_restricts_which_hosts_are_contacted() {
        let d = tmp("viahost");
        let oa = net_in(&d, &["--via", "oa"]);
        let e = oa.get_bytes("https://arxiv.org/html/1706.03762", Ttl::Doc).await.unwrap_err();
        assert!(!is_net_err(&e) && e.contains("--via oa"), "{e}");
        let e = oa.get_bytes("https://export.arxiv.org/api/query?id_list=1", Ttl::Api).await.unwrap_err();
        assert!(e.contains("--via oa"), "{e}");
        let api = net_in(&d, &["--via", "api"]);
        let e = api.get_bytes("https://api.openalex.org/works?x=1", Ttl::Api).await.unwrap_err();
        assert!(e.contains("--via api"), "{e}");
        let e = api.get_bytes("https://api.semanticscholar.org/graph/v1/paper/arXiv:1/citations", Ttl::Doc).await.unwrap_err();
        assert!(e.contains("--via api"), "{e}");
        // cached copies of the other source are still readable
        oa.cache_put("https://arxiv.org/html/1", Some(b"cached"));
        assert_eq!(oa.get_bytes("https://arxiv.org/html/1", Ttl::Doc).await, Ok(Some(b"cached".to_vec())));
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn stale_offline_reads_mark_the_run_degraded() {
        let d = tmp("staleoff");
        let n = net_in(&d, &["--via", "cache"]);
        n.cache_put("https://example.invalid/old", Some(b"x"));
        n.cache_put("https://example.invalid/new", Some(b"y"));
        let f = n.cache_file("https://example.invalid/old").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(10 * TTL_API);
        fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
        assert_eq!(n.get_bytes_ex("https://example.invalid/new", Ttl::Api).await, Ok((Some(b"y".to_vec()), false)));
        assert!(!n.degraded());
        assert_eq!(n.get_bytes_ex("https://example.invalid/old", Ttl::Api).await, Ok((Some(b"x".to_vec()), true)));
        assert!(n.degraded() && n.take_notes().iter().any(|m| m.contains("stale")));
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn offline_mode_and_cooling_never_touch_the_network() {
        let d = tmp("offline");
        let n = net_in(&d, &["--via", "cache"]);
        n.cache_put("https://example.invalid/a", Some(b"cached"));
        assert_eq!(n.get_bytes("https://example.invalid/a", Ttl::Api).await, Ok(Some(b"cached".to_vec())));
        let e = n.get_bytes("https://example.invalid/b", Ttl::Api).await.unwrap_err();
        assert!(is_net_err(&e) && e.contains("not cached"), "{e}");
        // a cooling host fails fast and serves stale data when it has some
        let m = net_in(&d, &[]);
        m.trip("example.invalid", Some(600), false);
        m.cache_put("https://example.invalid/c", Some(b"old"));
        assert_eq!(m.get_bytes("https://example.invalid/c", Ttl::Api).await, Ok(Some(b"old".to_vec())));
        let e = m.get_bytes("https://example.invalid/d", Ttl::Api).await.unwrap_err();
        assert!(is_net_err(&e) && e.contains("cooling down"), "{e}");
        let _ = fs::remove_dir_all(&d);
    }
}
