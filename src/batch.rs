//! Compatibility helpers for file batches. New embedders should keep a
//! `http::Client` to retain its pool across calls and obtain per-item results.
//! The native batch path has no subprocess command line or ARG_MAX limit.

use std::path::{Path, PathBuf};


/// One thing to fetch and where it goes.
pub struct Item {
    pub url: String,
    pub dest: PathBuf,
}

pub struct BatchOpts {
    /// Concurrent HTTP requests. A separate default cap of four applies to each origin.
    pub parallel: usize,
    /// Additional attempts per file. The engine applies backoff and Retry-After.
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
#[cfg(test)]
const ARG_BUDGET: usize = 96 * 1024;

/// Legacy public helper, retained for source compatibility. Native batches do not use it.
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

/// Fetch every item, reusing connections within the call. `progress` receives (done, total)
/// on completion and at most every 50ms while waiting; false cancels active and queued items.
/// Failed indexes always refer to input order. Use http::Client for detailed results,
/// per-origin configuration, an external cancellation token and pooling across calls.
pub fn fetch_many(
    items: &[Item],
    opts: &BatchOpts,
    mut progress: impl FnMut(usize, usize) -> bool,
) -> Result<BatchResult, String> {
    let options = crate::http::Options {
        parallel: opts.parallel,
        retries: opts.retries,
        user_agent: opts.user_agent.clone(),
        connect_timeout: std::time::Duration::from_secs(opts.connect_timeout as u64),
        timeout: std::time::Duration::from_secs(opts.max_time as u64),
        ..Default::default()
    };
    let results = crate::http::Client::new()?.fetch_many(items, &options,
        &crate::http::Cancellation::new(), &mut progress)?;
    let failed_indexes: Vec<usize> = results.iter().filter(|r| r.status != crate::http::Status::Ok)
        .map(|r| r.index).collect();
    Ok(BatchResult { ok: items.len() - failed_indexes.len(), failed: failed_indexes.len(), failed_indexes })
}

/// Convenience for the common shape: a list of URLs into one directory, named by their last path
/// segment.
pub fn fetch_into(urls: &[String], dir: &Path, opts: &BatchOpts,
                  progress: impl FnMut(usize, usize) -> bool) -> Result<BatchResult, String> {
    let items: Vec<Item> = urls.iter().map(|u| {
        let name = u.rsplit('/').next().unwrap_or("download");
        let name = name.split(['?', '#']).next().unwrap_or(name);
        Item { url: u.clone(), dest: dir.join(if name.is_empty() || name == "." || name == ".." { "download" } else { name }) }
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
