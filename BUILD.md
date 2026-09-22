# Building and testing dnengine

For whoever picks this up next, including Codex.

## Layout

```
src/lib.rs       the engine loop: threads, the shared plan, what happens to a block when an
                 attempt comes back short, empty or late
src/sched.rs     WHO FETCHES WHAT. Pure functions over a snapshot, each with a test that states
                 an exact situation. Read this first; it is the whole idea.
src/transport.rs the only part that talks to a network. A trait, so a native io_uring transport
                 can replace curl without anything above it changing.
src/probe.rs     what a server will allow, learned from a one byte ranged GET
src/state.rs     the resume record, and why it is not always trusted
src/netif.rs     the machine's ways onto the internet
src/serve.rs     the engine over a socket, for machines that are not where the bandwidth is
src/ffi.rs       the C ABI. Only primitives cross it, and a panic never unwinds into the caller
src/sha256.rs    FIPS 180-4, portable and SHA extension paths, checked against each other
include/         dnengine.h, the whole contract for non-Rust callers
examples/        C, Go, Python, Rust and shell, all doing the same download
```

## Build

```sh
cargo build --release
# target/release/dn               the CLI
# target/release/libdnengine.so   the C ABI, for everything else
# target/release/libdnengine.a    same, static
```

No dependencies, deliberately: this is what `arx` will use to fetch packages on a machine that
has just been installed. Adding a crate to it is a decision, not a convenience.

## Test

```sh
cargo test        # 23 unit tests, no network
```

The engine itself is tested end to end in a throwaway VM against a real HTTP server, never on
the host. The harness:

```sh
# on the host: a range capable server (python's SimpleHTTPServer IGNORES Range and will make
# the engine look broken), optionally with SLOW=1 to make one block in four crawl
python3 serve.py <dir> 8099
SLOW=1 python3 serve.py <dir> 8098

# the VM reaches the host at 10.0.2.2 through slirp; virtio_net is a module on Arch, so the
# initramfs has to insmod failover, net_failover and virtio_net before configuring eth0
qemu-system-x86_64 -enable-kvm -m 4096 -smp 4 -nographic -no-reboot \
  -kernel /boot/vmlinuz-linux -initrd out/initramfs.cpio \
  -append "console=ttyS0 panic=1 quiet rdinit=/init" \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0
```

The five cases that must keep passing:

1. 240 MB over 8 connections, sha256 matching exactly.
2. an interrupted download resumes: kill it, check the record lists finished blocks, run the
   same command and let it verify.
3. two mirrors used together while one of them crawls, still verified.
4. the same download driven through the C library (`examples/simple.c`).
5. a deliberately wrong `--expect` is refused, exit non-zero.

## What is easy to get wrong here

* **Never pin to an interface when there is only one.** Binding curl to `wlan0` cannot reach
  `127.0.0.1`, and it fights VPN and policy routes. Pinning only pays when there are several
  interfaces to add up; `is_local()` also keeps loopback targets unpinned.
* **A hedge must never start while a block is still waiting.** That is the rule that keeps a
  second attempt from stealing bandwidth from work that has not been done at all. The test
  `hedging_never_steals_from_work_that_still_needs_doing` guards it.
* **A block must never be stranded.** The avoid rule skips a lane that failed a block only while
  another lane is free; with nothing else free the block is taken anyway.
* **Mirrors that disagree about the size are not the same file.** `run()` drops any source whose
  probe disagrees, rather than stitching ranges from two builds into one corrupt file that still
  looks complete.
* **The server binds localhost and requires a token otherwise**, and a request chooses a name,
  never a path. Both are tested; do not relax either for convenience.
* **No em-dashes** in anything user facing, and commits are authored
  `0xb0rn3 | スティングレイ` with no trailers.
