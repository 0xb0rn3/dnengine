//! dnengine: a download engine that uses every connection you have.
//!
//! One file, many connections, every network the machine has, and every mirror that serves it,
//! all at once. Resumable, and verified against a hash when you have one.
//!
//! The layering, which is what makes this a shared engine rather than a downloader:
//!
//!   1. transport   one byte range over one connection. Today that is curl, which is on every
//!                  machine and brings HTTP/2, proxies, redirects and TLS with it. The trait is
//!                  here so a native io_uring transport can replace it without touching
//!                  anything above.
//!   2. scheduling  which connection fetches which block, including hedging a slow one. Ported
//!                  from Plexo (MIT); see sched.rs.
//!   3. lanes       a mirror paired with a network interface. Many mirrors and many interfaces
//!                  multiply into lanes, and the scheduler routes around whichever is bad.
//!   4. state       what has already landed, so an interrupted download continues rather than
//!                  starting again.
//!   5. integrity   the bytes are hashed as they are written, and checked against what the
//!                  project published.
//!
//! This is meant to be used by other programs rather than typed by a person, the way aria2 or
//! libcurl are: link the crate, link the C ABI, run the `dn` binary, or drive it over a socket.
//! It is std only on purpose, so it builds on a machine with nothing installed and an empty cargo
//! cache, which is often exactly the machine that needs to fetch something.

pub mod batch;
pub mod ffi;
pub mod json;
pub mod netif;
pub mod probe;
pub mod sched;
pub mod serve;
pub mod sha256;
pub mod state;
pub mod transport;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sched::{Lane, Plan, Status, Work};
use transport::Transport;

/// What the caller watches while it runs.
#[derive(Clone, Debug)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub bytes_per_second: u64,
    pub connections: usize,
    /// Bytes fetched twice because a hedge raced a slow connection. Small, and worth knowing.
    pub duplicated: u64,
}

pub struct Download {
    urls: Vec<String>,
    dest: PathBuf,
    streams_per_lane: usize,
    max_connections: usize,
    networks: Option<Vec<String>>,
    block_size: u64,
    expect: Option<String>,
    resume: bool,
}

impl Download {
    /// One or more URLs for the SAME file. More than one is a strength: they become lanes.
    pub fn new(urls: Vec<String>, dest: impl Into<PathBuf>) -> Download {
        Download {
            urls, dest: dest.into(), streams_per_lane: 2, max_connections: 8,
            networks: None, block_size: 0, expect: None, resume: true,
        }
    }
    pub fn streams_per_lane(mut self, n: usize) -> Self { self.streams_per_lane = n.clamp(1, 16); self }
    pub fn max_connections(mut self, n: usize) -> Self { self.max_connections = n.clamp(1, 64); self }
    pub fn networks(mut self, nets: Vec<String>) -> Self { self.networks = Some(nets); self }
    pub fn block_size(mut self, n: u64) -> Self { self.block_size = n; self }
    pub fn expect_sha256(mut self, sha: impl Into<String>) -> Self { self.expect = Some(sha.into()); self }
    pub fn resume(mut self, yes: bool) -> Self { self.resume = yes; self }

    /// Run it. `on_progress` is called a few times a second, never on a hot path.
    pub fn run<T: Transport + Sync, F: FnMut(&Progress)>(
        self, transport: &T, mut on_progress: F,
    ) -> Result<Outcome, String> {
        if self.urls.is_empty() { return Err("no url".into()); }

        // Probe every mirror. One that reports a different size is not serving the same file,
        // and mixing ranges from it would produce a corrupt result that still looks complete.
        let probes: Vec<(String, probe::Probe)> = self.urls.iter()
            .map(|u| (u.clone(), probe::probe(u)))
            .collect();
        let size = probes.iter().map(|(_, p)| p.size).max().unwrap_or(0);
        if size == 0 { return Err("the server did not say how big the file is".into()); }
        let sources: Vec<String> = probes.iter()
            .filter(|(_, p)| p.size == size && p.ranges)
            .map(|(u, _)| u.clone()).collect();
        if sources.is_empty() {
            return Err("this file cannot be fetched in ranges (single connection is the only option)".into());
        }

        // Pinning a connection to an interface is only worth anything when there are several to
        // add up. With one, it can only break things: binding to wlan0 cannot reach 127.0.0.1,
        // and it also fights VPN and policy routes. An empty name means "let the routing table
        // decide", which is what a single-homed machine wants.
        let nets = match &self.networks {
            Some(n) if !n.is_empty() => n.clone(),
            _ => {
                let found = netif::usable();
                if found.len() > 1 && !is_local(&sources[0]) { found } else { vec![String::new()] }
            }
        };

        // lanes = mirrors x interfaces, then streams on each, capped so we stay a good citizen
        let mut lanes: Vec<Lane> = Vec::new();
        for (i, _) in sources.iter().enumerate() {
            for n in &nets { lanes.push(Lane { source: i, net: n.clone() }); }
        }
        let mut streams: Vec<(usize, Lane)> = Vec::new();
        'outer: for _ in 0..self.streams_per_lane {
            for l in &lanes {
                if streams.len() >= self.max_connections { break 'outer; }
                streams.push((streams.len(), l.clone()));
            }
        }
        if streams.is_empty() { return Err("no usable lane".into()); }

        let file = OpenOptions::new().create(true).write(true).read(true).open(&self.dest)
            .map_err(|e| format!("cannot open {}: {e}", self.dest.display()))?;
        file.set_len(size).map_err(|e| format!("cannot size {}: {e}", self.dest.display()))?;
        let file = Arc::new(file);

        let mut blocks = sched::plan_blocks(size, streams.len(), self.block_size);
        let resumed_list = if self.resume {
            state::load(&self.dest, &self.urls[0], size)
        } else { Vec::new() };
        for i in &resumed_list {
            if let Some(b) = blocks.get_mut(*i) { b.status = Status::Done; b.got = b.len(); }
        }

        let plan = Arc::new(Mutex::new(Plan::new(blocks)));
        {
            let mut p = plan.lock().unwrap();
            for (_, l) in &streams { *p.idle.entry(l.key()).or_insert(0) += 1; }
        }
        let fetched = Arc::new(AtomicU64::new(0));      // bytes off the network, hedges included
        let duplicated = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let started = Instant::now();

        std::thread::scope(|scope| {
            for (id, lane) in streams.iter().cloned() {
                let (plan, file, fetched, duplicated, stop, failure) =
                    (Arc::clone(&plan), Arc::clone(&file), Arc::clone(&fetched),
                     Arc::clone(&duplicated), Arc::clone(&stop), Arc::clone(&failure));
                let (sources, dest, url0) = (sources.clone(), self.dest.clone(), self.urls[0].clone());
                scope.spawn(move || {
                    loop {
                        if stop.load(Ordering::Relaxed) { return; }
                        let (index, from, to, hedged) = {
                            let mut p = plan.lock().unwrap();
                            if p.finished() { return; }
                            match p.pick(id, &lane, Instant::now()) {
                                Work::Nothing => {
                                    drop(p);
                                    std::thread::sleep(Duration::from_millis(150));
                                    continue;
                                }
                                w => {
                                    let (i, hedged) = match w {
                                        Work::Primary(i) => (i, false),
                                        Work::Hedge(i) => (i, true),
                                        Work::Nothing => unreachable!(),
                                    };
                                    if hedged { p.blocks[i].hedges += 1; }
                                    let b = &mut p.blocks[i];
                                    b.status = Status::Running;
                                    b.attempts.push(sched::Attempt {
                                        stream: id, lane: lane.clone(), started: Instant::now() });
                                    let range = (b.start + if hedged { 0 } else { b.got }, b.end);
                                    if let Some(c) = p.idle.get_mut(&lane.key()) { *c = c.saturating_sub(1); }
                                    (i, range.0, range.1, hedged)
                                }
                            }
                        };

                        let file2 = Arc::clone(&file);
                        let fetched2 = Arc::clone(&fetched);
                        let plan2 = Arc::clone(&plan);
                        let result = transport.range(
                            &sources[lane.source], &lane.net, from, to, &stop,
                            &mut |offset: u64, bytes: &[u8]| {
                                use std::os::unix::fs::FileExt;
                                file2.write_at(bytes, offset)?;
                                fetched2.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                                Ok(())
                            },
                            &mut |bps: f64| { plan2.lock().unwrap().speed.insert(id, bps); },
                        );

                        let mut p = plan.lock().unwrap();
                        *p.idle.entry(lane.key()).or_insert(0) += 1;
                        let b = &mut p.blocks[index];
                        b.attempts.retain(|a| a.stream != id);
                        match result {
                            Ok(n) => {
                                if b.status == Status::Done {
                                    duplicated.fetch_add(n, Ordering::Relaxed); // a hedge lost the race
                                } else if hedged {
                                    // a hedge fetched the whole block from its start
                                    duplicated.fetch_add(b.got.min(n), Ordering::Relaxed);
                                    b.got = n;
                                    if b.got >= b.len() { b.status = Status::Done; b.attempts.clear(); }
                                    else if b.attempts.is_empty() { b.status = Status::Pending; }
                                } else {
                                    b.got += n;
                                    if b.got >= b.len() { b.status = Status::Done; b.attempts.clear(); }
                                    else if b.attempts.is_empty() { b.status = Status::Pending; }
                                }
                                if b.status == Status::Done {
                                    let done: Vec<usize> = p.blocks.iter()
                                        .filter(|x| x.status == Status::Done).map(|x| x.index).collect();
                                    drop(p);
                                    state::save(&dest, &url0, size, &done);
                                    continue;
                                }
                            }
                            Err(e) => {
                                let give_up = if b.status != Status::Done {
                                    if b.got == 0 { b.avoid = Some(lane.key()); b.empties += 1; }
                                    if b.attempts.is_empty() { b.status = Status::Pending; }
                                    b.empties > sched::MAX_EMPTY_ATTEMPTS
                                } else { false };
                                drop(p);
                                let mut f = failure.lock().unwrap();
                                if f.is_none() { *f = Some(e); }
                                drop(f);
                                if give_up { stop.store(true, Ordering::Relaxed); return; }
                            }
                        }
                    }
                });
            }

            // the caller's view, updated a few times a second from the plan itself
            let mut last = (Instant::now(), 0u64);
            loop {
                let (done, finished) = {
                    let p = plan.lock().unwrap();
                    (p.done_bytes(), p.finished())
                };
                let now = Instant::now();
                let dt = now.duration_since(last.0).as_secs_f64();
                let rate = if dt > 0.2 {
                    let r = (done.saturating_sub(last.1)) as f64 / dt;
                    last = (now, done);
                    r
                } else { 0.0 };
                on_progress(&Progress {
                    done, total: size, bytes_per_second: rate as u64,
                    connections: streams.len(), duplicated: duplicated.load(Ordering::Relaxed),
                });
                if finished || stop.load(Ordering::Relaxed) { break; }
                std::thread::sleep(Duration::from_millis(200));
            }
            stop.store(true, Ordering::Relaxed);
        });

        let complete = plan.lock().unwrap().finished();
        let done = plan.lock().unwrap().done_bytes();
        on_progress(&Progress { done, total: size, bytes_per_second: 0,
                                connections: streams.len(),
                                duplicated: duplicated.load(Ordering::Relaxed) });
        if !complete {
            let why = failure.lock().unwrap().clone()
                .unwrap_or_else(|| "the download did not finish".into());
            return Err(format!("{why} ({done} of {size} bytes; run it again to resume)"));
        }

        // integrity last, over what actually landed on disk
        let sha = sha256::file(&self.dest).map_err(|e| format!("cannot read back {}: {e}", self.dest.display()))?;
        if let Some(want) = &self.expect {
            if !want.trim().eq_ignore_ascii_case(&sha) {
                return Err(format!("what arrived is not what was published\n  published {want}\n  arrived   {sha}"));
            }
        }
        state::clear(&self.dest);
        Ok(Outcome {
            path: self.dest, size, sha256: sha, seconds: started.elapsed().as_secs_f64(),
            connections: streams.len(), mirrors: sources.len(),
            duplicated: duplicated.load(Ordering::Relaxed),
        })
    }
}

pub struct Outcome {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub seconds: f64,
    pub connections: usize,
    pub mirrors: usize,
    pub duplicated: u64,
}

/// A target on this machine is never reached over a pinned interface.
fn is_local(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or(url)
        .split('/').next().unwrap_or("")
        .rsplit('@').next().unwrap_or("")
        .split(':').next().unwrap_or("");
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// Bytes, grouped, because this is the number someone watches to know it is moving.
pub fn commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 { out.push(','); }
        out.push(c);
    }
    out
}

pub fn human(bytes: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 { v /= 1024.0; i += 1; }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", U[i]) }
}

/// A single connection, for servers that will not do ranges. Kept here so callers have one
/// entry point for "get me this file" whatever the server supports.
pub fn single<T: Transport>(transport: &T, url: &str, dest: &Path) -> Result<(), String> {
    let stop = AtomicBool::new(false);
    let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(dest)
        .map_err(|e| format!("cannot open {}: {e}", dest.display()))?;
    let mut at = 0u64;
    transport.whole(url, "", &stop, &mut |bytes: &[u8]| {
        file.write_all(bytes)?;
        at += bytes.len() as u64;
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_on_this_machine_is_never_pinned_to_an_interface() {
        for u in ["http://127.0.0.1:8099/x", "http://localhost/x", "https://user@127.0.0.5:1/x"] {
            assert!(is_local(u), "{u} is this machine");
        }
        for u in ["https://mirror.example/x", "http://10.0.2.2:8099/x"] {
            assert!(!is_local(u), "{u} is not");
        }
    }

    #[test]
    fn byte_counts_are_grouped_the_way_people_read_them() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(4_223_172_608), "4,223,172,608");
    }
}
