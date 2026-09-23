# dnengine

A download engine that uses every connection you have. One file, many connections, every network
interface on the machine, and every mirror that serves it, all at once. Resumable, and checked
against a hash.

It is meant to be used by other programs, the way aria2 or libcurl are: link it, call the `dn`
binary, or point a thin client at it over a socket. It is std-only Rust with no crate
dependencies, so it builds on a machine with nothing installed and an empty cargo cache, and it
exposes a plain C ABI so C, C++, Go, Python and anything else with an FFI can use the same
engine.

```sh
dn get https://mirror.example/big.iso -o big.iso --expect 18d87568...
dn get https://a/x.iso https://b/x.iso -o x.iso      # two mirrors, used together
dn probe https://mirror.example/big.iso              # what the server will allow
dn net                                               # the ways onto the internet you have
dn serve --dir /srv/pool --token hunter2             # lend the engine to other machines
```

## Why it is faster

A single connection to a mirror is almost never as fast as your link: the mirror shapes per
connection, and TCP takes a while to find the ceiling. So the file is cut into blocks and the
blocks are pulled in parallel. Two things follow from that:

* **Several mirrors at once.** Give it more than one URL for the same file and they become
  lanes. A mirror that throttles simply gets fewer blocks.
* **Several networks at once.** Wifi, ethernet and a tethered phone are three ways onto the
  internet, and blocks go out over all of them, so the speeds add up rather than the routing
  table picking one.

And one thing does not follow: if your own link is the bottleneck, none of this helps. Measured
here on a saturated 1.7 MB/s wifi link, parallel fetching was no faster than a single curl, which
is the correct and honest result. It pays off against throttling mirrors, on lossy links where one
connection stalls, and on machines with more than one way out.

## Next to the usual tools

| | dnengine | curl / wget | aria2 |
| --- | --- | --- | --- |
| several connections to one file | yes | no | yes |
| several mirrors combined | yes | no | yes |
| several **network interfaces** at once | yes | no | no |
| hash checked after the download | built in | you do it | yes |
| usable as a library | Rust crate, C ABI, or a socket | libcurl | RPC |
| dependencies | none | none / none | several |

The interface part is the unusual one. Wifi and ethernet and a tethered phone are three ways onto
the internet, and this uses them together for a single file rather than whichever one the routing
table picked.

## What it does when something goes wrong

* **A slow connection gets raced.** Once nothing is left waiting, a second attempt starts on the
  block whose holder will take longest, on a different lane. Whichever finishes first wins. It
  can never take bandwidth from work that still needs doing, because it only happens when there
  is none left.
* **A lane that delivers nothing is avoided** for that block while another lane is free to take
  it, so a dead mirror cannot keep grabbing the same block while healthy ones idle.
* **A connection that goes quiet for 20 seconds is dropped** and its block requeued. A hung
  connection is worse than a failed one.
* **An interruption is not a loss.** Finished blocks are recorded beside the file, so running the
  same command again continues. The record is only trusted for the same url and size, so a mirror
  that has since published a new build cannot be stitched into the old one.
* **The result is verified.** `--expect <sha256>` refuses a file that is not what the project
  published, after it lands and before you use it.

## Three ways to call it

See [INTEGRATION.md](INTEGRATION.md) for the full guide and [examples/](examples) for working code
in five languages.

| | how | when |
| --- | --- | --- |
| **compiled in** | link `dnengine` (Rust) or `libdnengine.so` (C ABI) | the progress belongs in your own UI |
| **local** | run `dn ... --json` and read a line at a time | any language, no build changes, nothing to link |
| **over the network** | `dn serve` on the machine with the bandwidth | the thing that wants the file is not where the line is |

## Install

```sh
cargo build --release
sudo install -Dm755 target/release/dn /usr/bin/dn
sudo install -Dm755 target/release/libdnengine.so /usr/lib/libdnengine.so
sudo install -Dm644 include/dnengine.h /usr/include/dnengine.h
```

## Tests

```sh
cargo test
```

23 unit tests covering the scheduling rules (a failed lane is avoided but a block is never
stranded; a hedge never starts while work is waiting; a nearly finished holder is left alone),
block planning, header parsing, the resume record, path containment in the server, and the two
SHA-256 engines against each other.

The engine itself is exercised end to end in a throwaway VM against a real HTTP server: 240 MB
over 8 connections with the hash checked, an interrupted download resumed from its record, two
mirrors used together with one of them deliberately crawling, the same download driven through
the C library, and a deliberately wrong hash refused.

## Credit

The scheduling rules, primary and hedge, come from
[Plexo](https://github.com/anmolkapil/plexo) (MIT, Copyright (c) 2026 Anmol Kapil), which states
them clearly and argues them well; `src/sched.rs` keeps that reasoning in its comments. Plexo is
TypeScript on Electron and races a file across network interfaces. This is std-only Rust, adds
mirrors as a second dimension of the same idea, and is built to be linked by other programs.

---

MIT. Built by Stingray Labs, and used by [ArxOS](https://arxos.uk) among others, but it belongs
to no particular distribution: if it only downloads files for you, it is doing its job.
