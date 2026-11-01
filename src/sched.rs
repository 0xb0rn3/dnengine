//! Who fetches what.
//!
//! The rules are decided from a snapshot of the download and nothing else: no I/O, no clock of
//! its own, so every one of them can be checked against an exact situation in a test.
//!
//! Ported from Plexo (https://github.com/anmolkapil/plexo, MIT, Copyright (c) 2026 Anmol Kapil),
//! which states them clearly and whose reasoning is worth keeping. Four kinds of work, in the
//! order they are handed out:
//!
//!   primary  a block nobody is fetching. The normal case, and the only one until the queue
//!            runs dry.
//!   gap      a stretch inside a block that nobody is fetching and nobody has: what a split
//!            left behind when it stopped early. Taken like a waiting block.
//!   hedge    a second attempt at a block someone else is fetching too slowly. Only handed out
//!            once no block is left waiting, so it can never take bandwidth from work that still
//!            needs doing, and only when this stream would clearly win. What it costs is bytes
//!            fetched twice; what it buys is that one slow connection can no longer hold the
//!            whole download back. Whichever attempt finishes first wins.
//!   split    the back half of the part someone is fetching, with the holder stopped where the
//!            split begins, so nothing is fetched twice. Tried after hedging, because racing a
//!            slow block is the cheaper answer when a block is nearly done.
//!
//! What is ours is the generalisation from "network" to LANE. Plexo races a file across the
//! machine's network interfaces; a lane here is an interface paired with one of several mirrors
//! serving the same bytes, so the same rules also route around a mirror that is throttling while
//! another one is not.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A block is hedged once every attempt at it has been going this long: long enough to have
/// measured a real speed, and far more than a new connection costs.
pub const HEDGE_AFTER: Duration = Duration::from_secs(8);
/// Bounds the duplicate work on a block whose hedges keep failing.
pub const MAX_HEDGES_PER_BLOCK: u32 = 2;
/// A block whose attempts keep coming back empty stops the download rather than being retried
/// for ever: something is wrong with the link or the mirror, and hiding that helps nobody.
pub const MAX_EMPTY_ATTEMPTS: u32 = 5;
/// What a new request costs before its first byte arrives (connecting, the server answering),
/// counted into how long a hedge would take.
pub const STARTUP: Duration = Duration::from_millis(500);
/// The least either half of a split may be. Each part is its own request, so below this the
/// round trip costs more than the split saves.
pub const MIN_SPLIT: u64 = 256 << 10;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status { Pending, Running, Done }

/// One connection's identity: which mirror, over which interface. An empty interface means
/// "whatever the routing table picks".
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Lane { pub source: usize, pub net: String }

impl Lane {
    pub fn key(&self) -> String { format!("{}#{}", self.source, self.net) }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind { Primary, Hedge, Split }

#[derive(Debug)]
pub struct Attempt {
    pub stream: usize,
    pub lane: Lane,
    pub started: Instant,
    pub kind: Kind,
    /// Where in the block this attempt began, as an offset into the block.
    pub at: u64,
    /// Where it stops, as an offset into the block, exclusive.
    pub end: u64,
    /// How many bytes it has received so far.
    pub got: u64,
    /// It is waiting on the writer, so it is as slow as the disk. Another attempt cannot beat
    /// the disk, so such a holder is never raced.
    pub write_waiting: bool,
}

impl Attempt {
    /// The offset it has reached in the block.
    pub fn reached(&self) -> u64 { self.at + self.got }
}

/// A stretch of a block, as offsets into it, [start, end).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span { pub start: u64, pub end: u64 }

#[derive(Debug)]
pub struct Block {
    pub index: usize,
    pub start: u64,
    pub end: u64, // inclusive
    pub status: Status,
    /// The frontier: the block is complete up to here, counted from its start.
    pub got: u64,
    /// A lane whose last attempt on this block delivered nothing.
    pub avoid: Option<String>,
    pub hedges: u32,
    pub empties: u32,
    pub attempts: Vec<Attempt>,
    /// What splits have fetched ahead of the frontier. A split's bytes are counted only once the
    /// frontier reaches them, so saved progress, restore and pause behaviour are unchanged: bytes
    /// fetched ahead are simply fetched again after a pause.
    pub ahead: Vec<Span>,
}

impl Block {
    pub fn len(&self) -> u64 { self.end - self.start + 1 }
    pub fn remaining(&self) -> u64 { self.len().saturating_sub(self.got) }

    /// Where an attempt starting `from` bytes into this block stops: where the next part that
    /// someone is fetching, or has already fetched, begins, or the end of the block. This is how
    /// a holder is stopped at a split without fetching anything twice.
    pub fn part_end(&self, from: u64) -> u64 {
        let mut end = self.len();
        for a in &self.attempts {
            if a.at > from { end = end.min(a.at); }
        }
        for s in &self.ahead {
            if s.start > from { end = end.min(s.start); }
        }
        end
    }

    /// Record `n` bytes fetched from offset `at`. Bytes at the frontier move it along; bytes
    /// fetched ahead of it are kept as a span and counted only once the frontier reaches them, so
    /// the frontier is always "complete up to here" and a pause loses time, never correctness.
    pub fn deliver(&mut self, at: u64, n: u64) {
        if n == 0 { return; }
        if at <= self.got { self.got = self.got.max((at + n).min(self.len())); }
        else { self.ahead.push(Span { start: at, end: (at + n).min(self.len()) }); }
        // absorb every span the frontier has now reached, which may chain
        self.ahead.sort_by_key(|s| s.start);
        while let Some(i) = self.ahead.iter().position(|s| s.start <= self.got && s.end > self.got) {
            self.got = self.ahead[i].end;
            self.ahead.remove(i);
        }
        self.ahead.retain(|s| s.end > self.got);
    }

    /// The first stretch past the frontier that nobody is fetching and nobody has: what a split
    /// left behind when it stopped early.
    fn first_gap(&self) -> Option<Span> {
        let len = self.len();
        let mut at = self.got;
        while at < len {
            // held or already fetched here? step over it and look again
            let taken = self.attempts.iter().find(|a| a.at <= at && at < a.end)
                .map(|a| a.end)
                .or_else(|| self.ahead.iter().find(|s| s.start <= at && at < s.end).map(|s| s.end));
            match taken {
                Some(next) if next > at => at = next,
                _ => return Some(Span { start: at, end: self.part_end(at) }),
            }
        }
        None
    }
}

pub struct Plan {
    pub blocks: Vec<Block>,
    /// How many streams are idle on each lane, so the scheduler can tell whether some other lane
    /// could take a block instead of the one that just failed at it.
    pub idle: HashMap<String, usize>,
    /// Measured speed per stream, in bytes per second.
    pub speed: HashMap<usize, f64>,
    /// The disk cannot keep up with what is arriving. Another attempt would only add writes, so
    /// no block is hedged or split while this holds.
    pub disk_behind: bool,
}

/// What a stream should do next. A range is always offsets into the block, `at` inclusive and
/// `end` exclusive.
#[derive(Debug, PartialEq)]
pub enum Work {
    Primary { index: usize, at: u64, end: u64 },
    Hedge { index: usize, at: u64, end: u64 },
    Split { index: usize, at: u64, end: u64 },
    Nothing,
}

impl Work {
    pub fn index(&self) -> Option<usize> {
        match *self {
            Work::Primary { index, .. } | Work::Hedge { index, .. } | Work::Split { index, .. } => Some(index),
            Work::Nothing => None,
        }
    }
    pub fn kind(&self) -> Option<Kind> {
        match self {
            Work::Primary { .. } => Some(Kind::Primary),
            Work::Hedge { .. } => Some(Kind::Hedge),
            Work::Split { .. } => Some(Kind::Split),
            Work::Nothing => None,
        }
    }
}

impl Plan {
    pub fn new(blocks: Vec<Block>) -> Plan {
        Plan { blocks, idle: HashMap::new(), speed: HashMap::new(), disk_behind: false }
    }

    pub fn finished(&self) -> bool { self.blocks.iter().all(|b| b.status == Status::Done) }

    pub fn done_bytes(&self) -> u64 { self.blocks.iter().map(|b| b.got.min(b.len())).sum() }

    fn anything_waiting(&self) -> bool {
        self.blocks.iter().any(|b| b.status == Status::Pending)
    }

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

    /// A stretch a split left behind. It is work that still needs doing, so it comes before any
    /// hedge or split, and the same lane history applies as for a waiting block.
    pub fn next_gap(&self, lane: &Lane) -> Option<Work> {
        let key = lane.key();
        for b in &self.blocks {
            if b.status != Status::Running { continue; }
            if b.avoid.as_deref() == Some(key.as_str()) { continue; }
            if let Some(g) = b.first_gap() {
                if g.end > g.start { return Some(Work::Primary { index: b.index, at: g.start, end: g.end }); }
            }
        }
        None
    }

    /// The block most worth another attempt: the one that will be longest yet, if this stream
    /// would clearly beat it. A block is as slow as its fastest attempt.
    pub fn next_hedge(&self, stream: usize, lane: &Lane, now: Instant) -> Option<Work> {
        // only when everything left is already being fetched, and the disk has room for more
        if self.disk_behind || self.anything_waiting() { return None; }
        let key = lane.key();
        let mut target = None;
        let mut latest = 0f64;
        for b in &self.blocks {
            if b.status != Status::Running { continue; }
            if b.attempts.iter().any(|a| a.stream == stream) { continue; }
            if b.hedges >= MAX_HEDGES_PER_BLOCK { continue; }
            if b.avoid.as_deref() == Some(key.as_str()) { continue; }
            // what is raced is the part fetched from the frontier; a split ahead of it goes on
            // by itself
            let racing: Vec<&Attempt> = b.attempts.iter().filter(|a| a.kind != Kind::Split).collect();
            // nothing to race if every attempt let go: the block goes back to the queue instead.
            // `continue`, never `?`: one such block must not end the search for the others.
            let Some(primary) = racing.iter().find(|a| a.kind == Kind::Primary) else { continue };
            // speeds only mean something once every attempt has been going a while
            if racing.iter().any(|a| now.duration_since(a.started) < HEDGE_AFTER) { continue; }
            // a holder waiting on its writer is as slow as the disk, which another attempt
            // cannot beat
            if racing.iter().any(|a| a.write_waiting) { continue; }
            let remaining = primary.end.saturating_sub(b.got);
            if remaining == 0 { continue; }
            let remaining = remaining as f64;
            // a holder that has gone quiet has no finish time at all
            let eta = racing.iter().map(|a| match self.speed.get(&a.stream) {
                Some(s) if *s > 1.0 => remaining / s,
                _ => f64::INFINITY,
            }).fold(f64::INFINITY, f64::min);
            // worth it only if this stream would finish in under half that time. One whose speed
            // is not known yet races a block that needs at least HEDGE_AFTER more.
            let worth_it = match self.speed.get(&stream) {
                Some(s) if *s > 1.0 => eta >= 2.0 * (STARTUP.as_secs_f64() + remaining / s),
                _ => eta >= HEDGE_AFTER.as_secs_f64(),
            };
            if !worth_it { continue; }
            // the holders' own lane may be what is slow, so a stream elsewhere gets the first go,
            // unless none of those would take it either
            let holders: Vec<String> = racing.iter().map(|a| a.lane.key()).collect();
            let other_free = self.idle.iter().any(|(k, n)| {
                !holders.contains(k) && *n > 0 && b.avoid.as_deref() != Some(k.as_str())
            });
            if holders.contains(&key) && other_free { continue; }
            if eta > latest { latest = eta; target = Some(Work::Hedge { index: b.index, at: b.got, end: primary.end }); }
        }
        target
    }

    /// The back half of the part with the most bytes left. Whoever is fetching across the split
    /// stops there, so nothing is fetched twice. No speeds come into it: racing a slow block is
    /// tried first. Splitting goes on while both halves stay at least MIN_SPLIT.
    pub fn next_split(&self, lane: &Lane) -> Option<Work> {
        if self.disk_behind || self.anything_waiting() { return None; }
        let key = lane.key();
        let mut target = None;
        let mut most = 2 * MIN_SPLIT - 1;
        for b in &self.blocks {
            if b.status != Status::Running { continue; }
            // a lane that got nothing from this block will not do better on a part of it
            if b.avoid.as_deref() == Some(key.as_str()) { continue; }
            // once a block is being raced, the race decides it: splitting the loser's part too
            // would hand out work that the winner is about to make pointless
            let raced = b.attempts.iter().any(|a| a.kind == Kind::Hedge);
            for a in &b.attempts {
                if raced && a.kind != Kind::Split { continue; }
                let reached = a.reached();
                let left = a.end.saturating_sub(reached);
                if left > most {
                    most = left;
                    target = Some(Work::Split { index: b.index, at: reached + left / 2, end: a.end });
                }
            }
        }
        target
    }

    pub fn pick(&self, stream: usize, lane: &Lane, now: Instant) -> Work {
        if let Some(i) = self.next_waiting(lane) {
            let b = &self.blocks[i];
            return Work::Primary { index: i, at: b.got, end: b.part_end(b.got) };
        }
        if let Some(w) = self.next_gap(lane) { return w; }
        if let Some(w) = self.next_hedge(stream, lane, now) { return w; }
        if let Some(w) = self.next_split(lane) { return w; }
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
                         avoid: None, hedges: 0, empties: 0, attempts: Vec::new(), ahead: Vec::new() });
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
            ahead: Vec::new(),
        }).collect())
    }

    /// One block of `len` bytes, running, so the split and gap rules have room to work.
    fn big(len: u64) -> Plan {
        let mut p = Plan::new(vec![Block {
            index: 0, start: 0, end: len - 1, status: Status::Running, got: 0, avoid: None,
            hedges: 0, empties: 0, attempts: Vec::new(), ahead: Vec::new(),
        }]);
        p.blocks[0].status = Status::Running;
        p
    }

    fn attempt(stream: usize, l: Lane, kind: Kind, at: u64, end: u64, got: u64, age: Duration) -> Attempt {
        Attempt { stream, lane: l, started: Instant::now() - age, kind, at, end, got,
                  write_waiting: false }
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
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100, 0,
                                          Duration::from_secs(60)));
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), None);
        assert_eq!(p.next_split(&lane(0, "eth0")), None, "nor does splitting");
    }

    #[test]
    fn a_slow_holder_is_hedged_once_nothing_is_waiting_but_only_so_often() {
        let mut p = plan(&[(Status::Running, None)]);
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100, 0,
                                          Duration::from_secs(60)));
        p.speed.insert(1, 1.0); // one byte a second: it will never finish
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()),
                   Some(Work::Hedge { index: 0, at: 0, end: 100 }));
        p.blocks[0].hedges = MAX_HEDGES_PER_BLOCK;
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), None);
    }

    #[test]
    fn a_holder_that_is_nearly_finished_is_left_alone() {
        let mut p = plan(&[(Status::Running, None)]);
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100, 0,
                                          Duration::from_secs(60)));
        p.speed.insert(1, 1_000_000.0);
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), None);
    }

    #[test]
    fn a_hedge_only_runs_when_this_stream_would_clearly_win() {
        let mut p = plan(&[(Status::Running, None)]);
        p.blocks[0].end = 0; // 1 byte block, resized below
        p.blocks[0].end = 10_000_000;
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 10_000_001, 0,
                                          Duration::from_secs(60)));
        p.speed.insert(1, 100_000.0);           // the holder needs about 100 s
        p.speed.insert(2, 110_000.0);           // barely faster: not worth a second request
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), None);
        p.speed.insert(2, 10_000_000.0);        // a hundred times faster: worth it
        assert!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()).is_some());
    }

    #[test]
    fn a_block_is_as_slow_as_its_fastest_attempt() {
        let mut p = plan(&[(Status::Running, None)]);
        p.blocks[0].end = 10_000_000;
        // one dead holder and one quick one: the quick one decides, so no third attempt
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 10_000_001, 0,
                                          Duration::from_secs(60)));
        p.blocks[0].attempts.push(attempt(3, lane(1, "eth0"), Kind::Hedge, 0, 10_000_001, 0,
                                          Duration::from_secs(60)));
        p.blocks[0].hedges = 1;
        p.speed.insert(1, 1.0);
        p.speed.insert(3, 10_000_000.0);
        p.speed.insert(2, 10_000_000.0);
        assert_eq!(p.next_hedge(2, &lane(0, "eth2"), Instant::now()), None);
    }

    #[test]
    fn a_holder_stalled_on_the_disk_is_not_raced() {
        let mut p = plan(&[(Status::Running, None)]);
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100, 0,
                                          Duration::from_secs(60)));
        p.blocks[0].attempts[0].write_waiting = true;
        p.speed.insert(1, 1.0);
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), None);
    }

    #[test]
    fn nothing_is_hedged_or_split_while_the_disk_is_behind() {
        let mut p = big(100 << 20);
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100 << 20, 1 << 20,
                                          Duration::from_secs(60)));
        p.speed.insert(1, 1.0);
        assert!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()).is_some());
        assert!(p.next_split(&lane(0, "eth0")).is_some());
        p.disk_behind = true;
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()), None);
        assert_eq!(p.next_split(&lane(0, "eth0")), None);
    }

    #[test]
    fn a_free_stream_takes_the_back_half_of_the_longest_part() {
        let mut p = big(100 << 20);
        // the holder has reached 20 MiB of a 100 MiB part: 80 MiB left, so the split starts at 60
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100 << 20, 20 << 20,
                                          Duration::from_secs(1)));
        assert_eq!(p.next_split(&lane(0, "eth0")),
                   Some(Work::Split { index: 0, at: 60 << 20, end: 100 << 20 }));
    }

    #[test]
    fn the_holder_stops_where_the_split_begins_so_nothing_is_fetched_twice() {
        let mut p = big(100 << 20);
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100 << 20, 20 << 20,
                                          Duration::from_secs(1)));
        let Some(Work::Split { at, .. }) = p.next_split(&lane(0, "eth0")) else { panic!("no split") };
        // once the split is in flight, the holder's part ends where it begins
        p.blocks[0].attempts.push(attempt(2, lane(0, "eth0"), Kind::Split, at, 100 << 20, 0,
                                          Duration::from_secs(0)));
        assert_eq!(p.blocks[0].part_end(0), at, "the holder is bounded by the split");
        assert_eq!(p.blocks[0].part_end(at), 100 << 20, "the split keeps its own end");
    }

    #[test]
    fn splitting_stops_when_either_half_would_be_too_small() {
        let mut p = big(100 << 20);
        // exactly twice the minimum is left: both halves are the minimum, so it still splits
        let left = 2 * MIN_SPLIT;
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, left, 0,
                                          Duration::from_secs(1)));
        assert_eq!(p.next_split(&lane(0, "eth0")),
                   Some(Work::Split { index: 0, at: MIN_SPLIT, end: left }));
        // one byte less and it does not
        p.blocks[0].attempts[0].end = left - 1;
        assert_eq!(p.next_split(&lane(0, "eth0")), None);
    }

    #[test]
    fn a_split_that_stopped_early_leaves_a_gap_the_next_stream_takes() {
        let mut p = big(100 << 20);
        // the frontier is at 10 MiB, a split fetched 60..80 MiB and stopped: 10..60 is nobody's
        p.blocks[0].got = 10 << 20;
        p.blocks[0].ahead.push(Span { start: 60 << 20, end: 80 << 20 });
        assert_eq!(p.next_gap(&lane(0, "eth0")),
                   Some(Work::Primary { index: 0, at: 10 << 20, end: 60 << 20 }));
        // with a holder on that stretch there is no gap until past what it has
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 10 << 20, 60 << 20, 0,
                                          Duration::from_secs(1)));
        assert_eq!(p.next_gap(&lane(0, "eth0")),
                   Some(Work::Primary { index: 0, at: 80 << 20, end: 100 << 20 }),
                   "the stretch past what the split fetched");
    }

    #[test]
    fn work_that_still_needs_doing_comes_before_a_hedge_or_a_split() {
        // block 0 has a stretch nobody holds; block 1 is held by a hopeless holder
        let mut p = big(100 << 20);
        p.blocks[0].got = 10 << 20;
        p.blocks[0].ahead.push(Span { start: 60 << 20, end: 100 << 20 });
        let mut b1 = Block {
            index: 1, start: 100 << 20, end: (200 << 20) - 1, status: Status::Running, got: 0,
            avoid: None, hedges: 0, empties: 0, attempts: Vec::new(), ahead: Vec::new(),
        };
        b1.attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100 << 20, 0,
                                 Duration::from_secs(60)));
        p.blocks.push(b1);
        p.speed.insert(1, 1.0);
        // a hedge and a split are both allowed here, but the gap is work nobody is doing
        assert!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()).is_some());
        assert!(p.next_split(&lane(0, "eth0")).is_some());
        assert_eq!(p.pick(2, &lane(0, "eth0"), Instant::now()),
                   Work::Primary { index: 0, at: 10 << 20, end: 60 << 20 });
    }

    #[test]
    fn a_block_with_no_holder_does_not_end_the_search_for_a_slow_one() {
        // block 0 has only a split running, so there is nothing to race on it; block 1 is slow
        let mut p = big(100 << 20);
        p.blocks[0].attempts.push(attempt(5, lane(1, "eth1"), Kind::Split, 50 << 20, 100 << 20, 0,
                                          Duration::from_secs(60)));
        let mut b1 = Block {
            index: 1, start: 100 << 20, end: (200 << 20) - 1, status: Status::Running, got: 0,
            avoid: None, hedges: 0, empties: 0, attempts: Vec::new(), ahead: Vec::new(),
        };
        b1.attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100 << 20, 0,
                                 Duration::from_secs(60)));
        p.blocks.push(b1);
        p.speed.insert(1, 1.0);
        assert_eq!(p.next_hedge(2, &lane(0, "eth0"), Instant::now()),
                   Some(Work::Hedge { index: 1, at: 0, end: 100 << 20 }));
    }

    #[test]
    fn a_raced_block_is_not_split_as_well() {
        let mut p = big(100 << 20);
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100 << 20, 0,
                                          Duration::from_secs(60)));
        assert!(p.next_split(&lane(0, "eth0")).is_some());
        p.blocks[0].attempts.push(attempt(3, lane(1, "eth1"), Kind::Hedge, 0, 100 << 20, 0,
                                          Duration::from_secs(1)));
        assert_eq!(p.next_split(&lane(0, "eth0")), None, "the race decides this block");
    }

    #[test]
    fn a_lane_that_got_nothing_from_a_block_is_not_given_part_of_it() {
        let mut p = big(100 << 20);
        p.blocks[0].avoid = Some("0#eth0".into());
        p.blocks[0].attempts.push(attempt(1, lane(0, "wlan0"), Kind::Primary, 0, 100 << 20, 0,
                                          Duration::from_secs(60)));
        assert_eq!(p.next_split(&lane(0, "eth0")), None);
        assert_eq!(p.next_gap(&lane(0, "eth0")), None);
        assert!(p.next_split(&lane(1, "eth1")).is_some(), "another lane may have it");
    }

    #[test]
    fn bytes_ahead_of_the_frontier_count_only_once_it_reaches_them() {
        let mut p = big(100 << 20);
        let b = &mut p.blocks[0];
        b.deliver(50 << 20, 10 << 20);                 // a split fetched 50..60 MiB
        assert_eq!(b.got, 0, "the frontier has not reached it");
        assert_eq!(b.ahead, vec![Span { start: 50 << 20, end: 60 << 20 }]);
        b.deliver(0, 50 << 20);                        // the holder arrives at the split
        assert_eq!(b.got, 60 << 20, "the span is absorbed");
        assert!(b.ahead.is_empty());
        // chained spans are absorbed together, and nothing runs past the end of the block
        b.deliver(70 << 20, 10 << 20);
        b.deliver(80 << 20, 30 << 20);
        b.deliver(60 << 20, 10 << 20);
        assert_eq!(b.got, 100 << 20);
        assert!(b.ahead.is_empty());
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
