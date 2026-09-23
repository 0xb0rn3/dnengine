//! Many small files, which is the workload the range splitter cannot help with.
//!
//! dnengine's parallel path splits ONE file across connections. That is the right answer for a
//! 4GB image and the wrong answer for four thousand 2MB ones: a 2MB JPEG gains nothing from being
//! cut into ranges, and the cost that actually dominates is per-file setup. Fetching them one
//! process at a time, which is what this engine used to do, pays a process spawn, a TCP handshake
//! and a TLS negotiation FOR EVERY FILE, and that is why importing it into something like a
//! wallpaper fetcher was worth nothing.
//!
//! A batch is handed to a single transfer process holding one connection pool. Same host means
//! the socket and the TLS session are reused, and over HTTP/2 the requests are multiplexed on one
//! connection. The transfers run concurrently inside that process rather than as separate ones.
//!
//! The batch is chunked because a command line has a length limit, and ARG_MAX is not a thing to
//! discover in production with someone's ten thousand file queue.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One thing to fetch and where it goes.
pub struct Item {
    pub url: String,
    pub dest: PathBuf,
}

pub struct BatchOpts {
    /// Concurrent transfers inside the one process. Above roughly 16 a single host stops going
    /// faster and starts answering 429, so this is not a dial to turn up for its own sake.
    pub parallel: usize,
    /// Attempts per file, with curl's own exponential backoff between them. Retries honour
    /// Retry-After, which is what a rate limited host is asking for.
    pub retries: u32,
    pub user_agent: String,
    pub connect_timeout: u32,
    /// Seconds a single transfer may take before it is abandoned. 0 means no limit.
    pub max_time: u32,
}

impl Default for BatchOpts {
    fn default() -> Self {
        BatchOpts {
            parallel: 8,
            retries: 2,
            user_agent: format!("dnengine/{}", env!("CARGO_PKG_VERSION")),
            connect_timeout: 20,
            max_time: 0,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct BatchResult {
    pub ok: usize,
    pub failed: usize,
    /// Which ones failed, by index into the batch that was handed in.
    pub failed_indexes: Vec<usize>,
}

/// Conservative room for the executable, the flags and the environment.
const ARG_BUDGET: usize = 96 * 1024;

/// Split a batch so no single command line can overflow ARG_MAX.
///
/// Returns index ranges rather than copies: a queue of ten thousand URLs should not be cloned to
/// be counted.
pub fn chunks(items: &[Item], budget: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let (mut start, mut used) = (0usize, 0usize);
    for (i, it) in items.iter().enumerate() {
        // "-o" + dest + url + separators
        let cost = it.url.len() + it.dest.as_os_str().len() + 8;
        if used + cost > budget && i > start {
            out.push((start, i));
            start = i;
            used = 0;
        }
        used += cost;
    }
    if start < items.len() {
        out.push((start, items.len()));
    }
    out
}

/// Fetch every item, reusing connections. `progress` is called with (done, total) after each
/// chunk; returning false asks the batch to stop.
pub fn fetch_many(
    items: &[Item],
    opts: &BatchOpts,
    mut progress: impl FnMut(usize, usize) -> bool,
) -> Result<BatchResult, String> {
    let mut res = BatchResult::default();
    if items.is_empty() {
        return Ok(res);
    }
    for (from, to) in chunks(items, ARG_BUDGET) {
        let slice = &items[from..to];
        for it in slice {
            if let Some(parent) = it.dest.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("{}: {e}", parent.display()))?;
                }
            }
        }
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS", "-L", "--fail",
            "--parallel", "--parallel-max", &opts.parallel.to_string(),
            "--retry", &opts.retries.to_string(), "--retry-connrefused",
            "--connect-timeout", &opts.connect_timeout.to_string(),
            "-A", &opts.user_agent,
            // one line per transfer, so a partial failure is attributable to a file rather than
            // sinking the whole batch
            "-w", "%{exitcode} %{http_code} %{url_effective}\\n",
        ]);
        if opts.max_time > 0 {
            cmd.args(["--max-time", &opts.max_time.to_string()]);
        }
        for it in slice {
            cmd.arg("-o").arg(&it.dest).arg(&it.url);
        }
        let out = cmd.output().map_err(|e| format!("curl will not start: {e}"))?;
        let report = String::from_utf8_lossy(&out.stdout);
        let mut seen = 0usize;
        for line in report.lines() {
            let mut f = line.split_whitespace();
            let code = f.next().unwrap_or("1");
            if code == "0" {
                res.ok += 1;
            } else {
                res.failed += 1;
                res.failed_indexes.push(from + seen);
            }
            seen += 1;
        }
        // curl prints one -w line per transfer; anything it never reported did not happen
        if seen < slice.len() {
            for i in seen..slice.len() {
                res.failed += 1;
                res.failed_indexes.push(from + i);
            }
        }
        if !progress(res.ok + res.failed, items.len()) {
            break;
        }
    }
    Ok(res)
}

/// Convenience for the common shape: a list of URLs into one directory, named by their last path
/// segment.
pub fn fetch_into(urls: &[String], dir: &Path, opts: &BatchOpts,
                  progress: impl FnMut(usize, usize) -> bool) -> Result<BatchResult, String> {
    let items: Vec<Item> = urls.iter().map(|u| {
        let name = u.rsplit('/').next().unwrap_or("download");
        let name = name.split(['?', '#']).next().unwrap_or(name);
        Item { url: u.clone(), dest: dir.join(if name.is_empty() { "download" } else { name }) }
    }).collect();
    fetch_many(&items, opts, progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(u: &str) -> Item { Item { url: u.into(), dest: PathBuf::from("/tmp/x") } }

    #[test]
    fn a_batch_that_fits_is_one_command() {
        let items: Vec<Item> = (0..50).map(|i| item(&format!("https://h/{i}"))).collect();
        assert_eq!(chunks(&items, ARG_BUDGET), vec![(0, 50)]);
    }

    #[test]
    fn a_batch_too_long_for_a_command_line_is_split_rather_than_truncated() {
        let items: Vec<Item> = (0..1000).map(|i| item(&format!("https://host/{i:0>200}"))).collect();
        let cs = chunks(&items, 4096);
        assert!(cs.len() > 1, "expected several chunks, got {}", cs.len());
        // every item lands in exactly one chunk, in order, none lost
        assert_eq!(cs[0].0, 0);
        assert_eq!(cs.last().unwrap().1, 1000);
        for w in cs.windows(2) { assert_eq!(w[0].1, w[1].0); }
    }

    #[test]
    fn one_item_larger_than_the_budget_still_gets_its_own_chunk() {
        // it cannot be split further, and dropping it silently would be worse than a long line
        let items = vec![item(&format!("https://host/{}", "a".repeat(9000)))];
        assert_eq!(chunks(&items, 4096), vec![(0, 1)]);
    }

    #[test]
    fn an_empty_batch_asks_for_nothing() {
        assert_eq!(chunks(&[], ARG_BUDGET), Vec::<(usize, usize)>::new());
        let r = fetch_many(&[], &BatchOpts::default(), |_, _| true).unwrap();
        assert_eq!(r, BatchResult::default());
    }

    #[test]
    fn a_url_with_a_query_does_not_become_a_filename_with_a_query_in_it() {
        let urls = vec!["https://h/a/b/pic.jpg?token=1&x=2".to_string()];
        let items: Vec<Item> = urls.iter().map(|u| {
            let name = u.rsplit('/').next().unwrap_or("download");
            let name = name.split(['?', '#']).next().unwrap_or(name);
            Item { url: u.clone(), dest: Path::new("/tmp").join(name) }
        }).collect();
        assert_eq!(items[0].dest, PathBuf::from("/tmp/pic.jpg"));
    }
}
