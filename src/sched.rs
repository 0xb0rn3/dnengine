//! Who fetches what.
//!
//! The rules are decided from a snapshot of the download and nothing else: no I/O, no clock of
//! its own, so every one of them can be checked against an exact situation in a test.
//!
//! Ported from Plexo (https://github.com/anmolkapil/plexo, MIT, Copyright (c) 2026 Anmol Kapil),
//! which states them clearly and whose reasoning is worth keeping:
//!
//!   primary  a block nobody is fetching. The normal case, and the only one until the queue
//!            runs dry.
//!   hedge    a second attempt at a block someone else is fetching too slowly. Only handed out
//!            once no block is left waiting, so it can never take bandwidth from work that still
//!            needs doing. What it costs is bytes fetched twice at the very end; what it buys is
//!            that one slow connection can no longer hold the whole download back. Whichever
//!            attempt finishes first wins and the other is dropped.
//!
//! What is ours is the generalisation from "network" to LANE. Plexo races a file across the
//! machine's network interfaces; a lane here is an interface paired with one of several mirrors
//! serving the same bytes, so the same rules also route around a mirror that is throttling while
//! another one is not.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A block is hedged once its holder has been at it this long and still needs as long again:
/// long enough to have measured a real speed, and far more than a new connection costs.
pub const HEDGE_AFTER: Duration = Duration::from_secs(8);
/// Bounds the duplicate work on a block whose hedges keep failing.
pub const MAX_HEDGES_PER_BLOCK: u32 = 2;
/// A block whose attempts keep coming back empty stops the download rather than being retried
/// for ever: something is wrong with the link or the mirror, and hiding that helps nobody.
pub const MAX_EMPTY_ATTEMPTS: u32 = 5;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status { Pending, Running, Done }

/// One connection's identity: which mirror, over which interface. An empty interface means
/// "whatever the routing table picks".
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Lane { pub source: usize, pub net: String }

impl Lane {
    pub fn key(&self) -> String { format!("{}#{}", self.source, self.net) }
}

#[derive(Debug)]
pub struct Attempt { pub stream: usize, pub lane: Lane, pub started: Instant }

#[derive(Debug)]
pub struct Block {
    pub index: usize,
    pub start: u64,
    pub end: u64, // inclusive
    pub status: Status,
    pub got: u64,
    /// A lane whose last attempt on this block delivered nothing.
    pub avoid: Option<String>,
    pub hedges: u32,
    pub empties: u32,
    pub attempts: Vec<Attempt>,
}

impl Block {
    pub fn len(&self) -> u64 { self.end - self.start + 1 }
    pub fn remaining(&self) -> u64 { self.len().saturating_sub(self.got) }
}

pub struct Plan {
    pub blocks: Vec<Block>,
    /// How many streams are idle on each lane, so the scheduler can tell whether some other lane
    /// could take a block instead of the one that just failed at it.
    pub idle: HashMap<String, usize>,
    /// Measured speed per stream, in bytes per second.
    pub speed: HashMap<usize, f64>,
}

#[derive(Debug, PartialEq)]
pub enum Work { Primary(usize), Hedge(usize), Nothing }

impl Plan {
    pub fn new(blocks: Vec<Block>) -> Plan {
        Plan { blocks, idle: HashMap::new(), speed: HashMap::new() }
    }

    pub fn finished(&self) -> bool { self.blocks.iter().all(|b| b.status == Status::Done) }

    pub fn done_bytes(&self) -> u64 { self.blocks.iter().map(|b| b.got.min(b.len())).sum() }

    /// The first waiting block, except one this lane already failed to deliver, which is left to
    /// another lane for as long as a stream there is free to take it. Otherwise a mirror that is
    /// not answering would be handed the same block again and again, by whichever of its streams
    /// asked first, while healthy ones sat idle beside it. With no other lane free the block is
    /// taken anyway, so it can never be stranded.
    pub fn next_waiting(&self, lane: &Lane) -> Option<usize> {
        let key = lane.key();
        let other_free = self.idle.iter().any(|(k, n)| k != &key && *n > 0);
        self.blocks.iter()
            .find(|b| b.status == Status::Pending && !(other_free && b.avoid.as_deref() == Some(key.as_str())))
            .map(|b| b.index)
    }

    /// The block most worth a second attempt: the one whose holder will be longest yet.
    pub fn next_hedge(&self, stream: usize, lane: &Lane, now: Instant) -> Option<usize> {
        // only when everything left is already being fetched
        if self.blocks.iter().any(|b| b.status == Status::Pending) { return None; }
        let key = lane.key();
        let mut target = None;
        let mut latest = 0f64;
        for b in &self.blocks {
            // lone primary only: one hedge at a time, and nothing to race if the holder let go
            if b.status != Status::Running || b.attempts.len() != 1 { continue; }
            let holder = &b.attempts[0];
            if holder.stream == stream { continue; }
            if b.hedges >= MAX_HEDGES_PER_BLOCK { continue; }
            if b.avoid.as_deref() == Some(key.as_str()) { continue; }
            if now.duration_since(holder.started) < HEDGE_AFTER { continue; }
            let remaining = b.remaining() as f64;
            if remaining <= 0.0 { continue; }
            // a holder that has gone quiet has no finish time at all
            let eta = match self.speed.get(&holder.stream) {
                Some(s) if *s > 1.0 => remaining / s,
                _ => f64::INFINITY,
            };
            if eta < HEDGE_AFTER.as_secs_f64() { continue; }
            // the holder's own lane may be what is slow, so a stream elsewhere gets the first go,
            // unless none of those would take it either
            let holder_key = holder.lane.key();
            let other_free = self.idle.iter()
                .any(|(k, n)| k != &holder_key && *n > 0 && b.avoid.as_deref() != Some(k.as_str()));
            if holder_key == key && other_free { continue; }
            if eta > latest { latest = eta; target = Some(b.index); }
        }
        target
    }

    pub fn pick(&self, stream: usize, lane: &Lane, now: Instant) -> Work {
        if let Some(i) = self.next_waiting(lane) { return Work::Primary(i); }
        if let Some(i) = self.next_hedge(stream, lane, now) { return Work::Hedge(i); }
        Work::Nothing
    }
}

/// Split a file into blocks: about four per stream, so one slow connection cannot be left
/// holding a huge tail while everything else has finished.
pub fn plan_blocks(size: u64, streams: usize, forced: u64) -> Vec<Block> {
    let block = if forced > 0 { forced } else {
        (size / (streams.max(1) as u64 * 4)).clamp(8 << 20, 64 << 20)
    };
    let mut out = Vec::new();
    let mut start = 0u64;
    while start < size {
        let end = std::cmp::min(start + block - 1, size - 1);
        out.push(Block { index: out.len(), start, end, status: Status::Pending, got: 0,
                         avoid: None, hedges: 0, empties: 0, attempts: Vec::new() });
        start = end + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(source: usize, net: &str) -> Lane { Lane { source, net: net.into() } }

    fn plan(states: &[(Status, Option<&str>)]) -> Plan {
        Plan::new(states.iter().enumerate().map(|(i, (s, avoid))| Block {
            index: i, start: i as u64 * 100, end: i as u64 * 100 + 99, status: *s, got: 0,
            avoid: avoid.map(String::from), hedges: 0, empties: 0, attempts: Vec::new(),
        }).collect())
    }

    #[test]
    fn a_lane_that_failed_a_block_leaves_it_to_another_one() {
        let mut p = plan(&[(Status::Pending, Some("0#wlan0")), (Status::Pending, None)]);
        p.idle.insert("0#eth0".into(), 1);
        assert_eq!(p.next_waiting(&lane(0, "wlan0")), Some(1), "skip the one it already failed");
        assert_eq!(p.next_waiting(&lane(0, "eth0")), Some(0), "no such history here");
    }

    #[test]
    fn the_same_interface_on_another_mirror_is_a_different_lane() {
        let mut p = plan(&[(Status::Pending, Some("0#wlan0"))]);
        p.idle.insert("1#wlan0".into(), 1);
        // mirror 0 failed this block; mirror 1 over the same interface is free, so it waits
        assert_eq!(p.next_waiting(&lane(0, "wlan0")), None);
        assert_eq!(p.next_waiting(&lane(1, "wlan0")), Some(0));
    }

    #[test]
    fn a_block_is_never_stranded_when_nothing_else_is_free() {
        let mut p = plan(&[(Status::Pending, Some("0#wlan0"))]);
        p.idle.insert("0#wlan0".into(), 1);
        assert_eq!(p.next_waiting(&lane(0, "wlan0")), Some(0));
    }

    #[test]
    fn hedging_never_steals_from_work_that_still_needs_doing() {
        let mut p = plan(&[(Status::Running, None), (Status::Pending, None)]);
        p.blocks[0].attempts.push(Attempt { stream: 1, lane: lane(0, "wlan0"),
                                            started: Instant::now() - Duration::from_secs(60) });
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), None);
    }

    #[test]
    fn a_slow_holder_is_hedged_once_nothing_is_waiting_but_only_so_often() {
        let mut p = plan(&[(Status::Running, None)]);
        p.blocks[0].attempts.push(Attempt { stream: 1, lane: lane(0, "wlan0"),
                                            started: Instant::now() - Duration::from_secs(60) });
        p.speed.insert(1, 1.0); // one byte a second: it will never finish
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), Some(0));
        p.blocks[0].hedges = MAX_HEDGES_PER_BLOCK;
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), None);
    }

    #[test]
    fn a_holder_that_is_nearly_finished_is_left_alone() {
        let mut p = plan(&[(Status::Running, None)]);
        p.blocks[0].attempts.push(Attempt { stream: 1, lane: lane(0, "wlan0"),
                                            started: Instant::now() - Duration::from_secs(60) });
        p.speed.insert(1, 1_000_000.0);
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), None);
    }

    #[test]
    fn blocks_cover_the_file_exactly_with_no_gap_or_overlap() {
        for size in [1u64, 100, 8 << 20, (8 << 20) + 1, 4_223_172_608] {
            let blocks = plan_blocks(size, 8, 0);
            assert_eq!(blocks[0].start, 0);
            assert_eq!(blocks.last().unwrap().end, size - 1);
            for pair in blocks.windows(2) {
                assert_eq!(pair[1].start, pair[0].end + 1, "gap or overlap at size {size}");
            }
            assert_eq!(blocks.iter().map(|b| b.len()).sum::<u64>(), size);
        }
    }
}
