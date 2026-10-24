//! HTTP engine: shared client, cross-process API rate gate, on-disk cache, retry/backoff.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::args::Args;

pub const API: &str = "https://export.arxiv.org/api/query";
const UA: &str = concat!("arx/", env!("CARGO_PKG_VERSION"), " (LLM-first arXiv CLI)");
const TTL_API: u64 = 3600;
const TTL_DOC: u64 = 7 * 86400;
const MAX_TRIES: u32 = 5;

#[derive(Clone, Copy)]
pub enum Ttl {
    Api,
    Doc,
}

/// Held for the duration of one API request (see `Net::gate`).
struct ApiGuard<'a> {
    _local: Option<tokio::sync::MutexGuard<'a, Option<std::time::Instant>>>,
    _file: Option<fs::File>,
}

pub struct Net {
    http: reqwest::Client,
    sem: Semaphore,
    /// In-process spacing (always enforced, even when the cross-process stamp file is unusable).
    local_gate: tokio::sync::Mutex<Option<std::time::Instant>>,
    gap_ms: u64,
    ttl_api: u64,
    ttl_doc: u64,
    fresh: bool,
    dir: Option<PathBuf>,
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

impl Net {
    pub fn new(a: &Args) -> Result<Net, String> {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .gzip(true)
            .brotli(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let par = a.num::<usize>("par").unwrap_or(12).clamp(1, 64);
        Ok(Net {
            http,
            sem: Semaphore::new(par),
            local_gate: tokio::sync::Mutex::new(None),
            gap_ms: a.num::<u64>("rate").unwrap_or(3000),
            ttl_api: a.num::<u64>("ttl").unwrap_or(TTL_API),
            ttl_doc: a.num::<u64>("ttl").unwrap_or(TTL_DOC),
            fresh: a.flag("fresh"),
            dir: cache_dir(),
        })
    }

    fn cache_path(&self, url: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("{:016x}", fnv(url))))
    }

    /// Ok(Some(Some(bytes))) hit, Ok(Some(None)) cached 404, None miss/stale.
    fn cache_get(&self, url: &str, ttl: u64) -> Option<Option<Vec<u8>>> {
        if self.fresh {
            return None;
        }
        let p = self.cache_path(url)?;
        let meta = fs::metadata(&p).ok()?;
        if meta.modified().ok()?.elapsed().ok()?.as_secs() > ttl {
            return None;
        }
        let mut buf = Vec::new();
        fs::File::open(&p).ok()?.read_to_end(&mut buf).ok()?;
        let nl = buf.iter().position(|&b| b == b'\n')?;
        if &buf[..nl] != url.as_bytes() {
            return None;
        }
        let body = &buf[nl + 1..];
        if body == b"\0404" {
            return Some(None);
        }
        Some(Some(body.to_vec()))
    }

    fn cache_put(&self, url: &str, body: Option<&[u8]>) {
        let Some(p) = self.cache_path(url) else { return };
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        let ok = (|| -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(url.as_bytes())?;
            f.write_all(b"\n")?;
            f.write_all(body.unwrap_or(b"\0404"))?;
            Ok(())
        })();
        if ok.is_ok() {
            let _ = fs::rename(&tmp, &p);
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Cross-process politeness gate: one API connection at a time across all arx processes, at least
    /// `gap_ms` between starts. The returned guard must be held until the response body is consumed.
    async fn gate(&self) -> ApiGuard<'_> {
        if self.gap_ms == 0 {
            return ApiGuard { _local: None, _file: None };
        }
        let mut last = self.local_gate.lock().await;
        let gap_d = Duration::from_millis(self.gap_ms);
        if let Some(t) = *last {
            let el = t.elapsed();
            if el < gap_d {
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
            let mut s = String::new();
            f.read_to_string(&mut s)?;
            let last: u128 = s.trim().parse().unwrap_or(0);
            let now = crate::dates::unix_ms();
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

    async fn fetch(&self, url: &str, api: bool) -> Result<Option<Vec<u8>>, String> {
        let mut last_err = String::new();
        for attempt in 0..MAX_TRIES {
            let gate = if api { Some(self.gate().await) } else { None };
            let _permit = if api { None } else { Some(self.sem.acquire().await.map_err(|e| e.to_string())?) };
            match self.http.get(url).send().await {
                Ok(r) => {
                    let st = r.status();
                    if st.is_success() {
                        match r.bytes().await {
                            Ok(b) => return Ok(Some(b.to_vec())),
                            Err(e) => {
                                // connection dropped mid-body: retry like any other transport failure
                                last_err = format!("{url}: {e}");
                                drop(_permit);
                                drop(gate);
                                if attempt + 1 < MAX_TRIES {
                                    tokio::time::sleep(Duration::from_millis(400 << attempt)).await;
                                }
                                continue;
                            }
                        }
                    }
                    if st.as_u16() == 404 {
                        return Ok(None);
                    }
                    last_err = format!("{url}: HTTP {st}");
                    if st.as_u16() == 429 || st.is_server_error() {
                        let ra = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
                        drop(_permit);
                        let d = ra.unwrap_or(1u64 << (attempt + 1)).min(30);
                        drop(gate); // never hold the connection slot while backing off
                        if attempt + 1 < MAX_TRIES {
                            tokio::time::sleep(Duration::from_secs(d)).await;
                        }
                        continue;
                    }
                    return Err(last_err);
                }
                Err(e) => {
                    last_err = format!("{url}: {e}");
                    drop(_permit);
                    drop(gate);
                    if attempt + 1 < MAX_TRIES {
                        tokio::time::sleep(Duration::from_millis(400 << attempt)).await;
                    }
                }
            }
        }
        Err(last_err)
    }

    /// GET with cache. `None` = 404.
    pub async fn get_bytes(&self, url: &str, ttl: Ttl) -> Result<Option<Vec<u8>>, String> {
        let api = url.starts_with(API);
        let secs = match ttl {
            Ttl::Api => self.ttl_api,
            Ttl::Doc => self.ttl_doc,
        };
        if let Some(hit) = self.cache_get(url, secs) {
            return Ok(hit);
        }
        let r = self.fetch(url, api).await?;
        self.cache_put(url, r.as_deref());
        Ok(r)
    }

    pub async fn get_text(&self, url: &str, ttl: Ttl) -> Result<Option<String>, String> {
        Ok(self.get_bytes(url, ttl).await?.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// arXiv API call.
    pub async fn api(&self, params: &[(&str, String)]) -> Result<String, String> {
        let url = api_url(params);
        self.get_text(&url, Ttl::Api).await?.ok_or_else(|| "arXiv API returned 404".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
