# Upstream Plexo, and what dnengine takes from it

Plexo (https://github.com/anmolkapil/plexo, MIT) is a TypeScript download manager. dnengine is a
Rust library and C ABI, so nothing is shared but the scheduling rules, which are stated clearly
there and worth keeping. Checked against upstream `main` at `e184f75` (2026-10-29).

## Carried (src/sched.rs)

| Upstream | What it says | State |
|---|---|---|
| #154 | Split the last blocks between free streams, and stop the holder at the split, so nothing is fetched twice. Both halves stay at least 256 KiB. | ported |
| #154 | A split that stops early leaves a gap; a free stream takes it like a waiting block. | ported (`first_gap`, `next_gap`) |
| #154 | A hedge races only the part fetched from the frontier; a split ahead of it goes on by itself. | ported |
| #152 | A stream that lost a race is free at once. | the download loop clears the attempt when a block is done |
| - | Hedge only when the asking stream would clearly win: the holder's finish time against the startup cost plus this stream's own rate. | ported (`STARTUP`) |
| - | A block is as slow as its fastest attempt. | ported |
| - | A holder waiting on its writer is as slow as the disk, so it is never raced. | ported (`write_waiting`) |
| - | With the disk behind, nothing is hedged or split: another attempt would only add writes. | ported (`disk_behind`) |

Bytes fetched ahead of the frontier are counted only once the frontier reaches them, so saved
progress, resume and pause behaviour do not change: bytes fetched ahead of a pause are fetched
again after it.

## Not carried, and why

- **Torrents over every network (#161), the DHT and peer search (#159).** dnengine fetches over
  HTTP(S). Torrents are not in its scope.
- **Per-connection proxy configuration (#32).** Worth taking. dnengine has one set of options for
  the whole client, so a proxy per lane is a change to `http::Options` as well as the scheduler.
- **Fewer, larger disk writes (#153).** Worth measuring here: the writer already hands each chunk
  to `write_all_at` at its own offset, which a split makes sparse, so gathering has to respect
  that.
- **Servers that give each byte range its own ETag (#158, pCloud).** Worth taking: one validator
  per range instead of one per file.
- **Redirects exhausted during probing (#125).** `probe.rs` should refuse rather than report the
  last hop.
- **The desktop interface, packaging and release machinery.** Plexo is an application; dnengine is
  a library.
