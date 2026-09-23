# Calling dnengine from your own program

Three ways in, and a short explanation of what the engine is doing so the choice makes sense.

```
                        your program
                             |
        +--------------------+--------------------+
        |                    |                    |
  compiled in            local process        over the network
  link the library       run `dn --json`      POST to `dn serve`
        |                    |                    |
        +--------------------+--------------------+
                             |
                      +------------------+
                      |  the engine      |
                      |  lanes: mirrors  |
                      |    x interfaces  |
                      |  blocks, hedging |
                      |  resume, verify  |
                      +------------------+
                             |
                      curl, one per range
```

## First, what it actually does

The file is cut into **blocks** of a few megabytes each. A **lane** is one mirror paired with one
network interface, so three mirrors and two interfaces make six lanes, and each lane runs a few
connections. A scheduler hands blocks to whichever connection is free.

That gives three wins at once: a throttling mirror gets fewer blocks, a second network adds its
bandwidth rather than sitting idle, and a stalled connection is raced by a second attempt instead
of holding everything up. Finished blocks are written down beside the file, so an interrupted
download continues. When the last block lands the whole file is hashed and, if you gave a hash,
refused unless it matches.

You never see any of that. You give it URLs and a destination, and it calls you back with numbers.

---

## 1. Compiled in

### Rust

```toml
[dependencies]
dnengine = { git = "https://github.com/0xb0rn3/dnengine" }
```

```rust
use dnengine::{transport, Download, human};

let outcome = Download::new(vec![url_a, url_b], "/tmp/x.iso")
    .streams_per_lane(4)
    .expect_sha256(published_hash)
    .run(&transport::Curl::default(), |p| {
        println!("{} of {} at {}/s", human(p.done), human(p.total), human(p.bytes_per_second));
    })?;
println!("{} in {:.0}s", outcome.sha256, outcome.seconds);
```

Full program: [examples/rust-app](examples/rust-app).

### C and C++

```sh
cc app.c -ldnengine -o app          # include/dnengine.h, target/release/libdnengine.so
```

```c
const char *urls[] = { "https://mirror/x.iso" };
dn_options opts;
memset(&opts, 0, sizeof opts);      /* zeroed means sensible defaults */
opts.streams_per_lane = 4;
opts.expect_sha256 = published_hash;

if (dn_download(urls, 1, "/tmp/x.iso", &opts, on_progress, NULL) != DN_OK)
    fprintf(stderr, "%s\n", dn_last_error());
```

Only primitives cross that boundary: NUL terminated strings, integers and one function pointer.
Returning non-zero from the progress callback cancels the download. A panic inside the engine is
caught and returned as `DN_ERR_PANIC` rather than unwinding into your frame. Full program:
[examples/simple.c](examples/simple.c).

### Go

cgo against the same header. [examples/app.go](examples/app.go).

### Python

`ctypes` against the same library, no package to install:

```python
dn = ctypes.CDLL("libdnengine.so")
dn.dn_download(urls, len(urls), dest, ctypes.byref(opts), PROGRESS(show), None)
```

Full module with a friendly wrapper: [examples/dn.py](examples/dn.py).

### Anything else

If the language can call C, it can call this: Zig, Ruby, Swift, Java through JNI or FFM, Node
through N-API or koffi. The header is the whole contract.

---

## 2. Local process

No linking, no build changes, works from any language that can start a program:

```sh
dn get "$URL" -o out.iso --expect "$SHA" --json
```

One JSON object per line on stdout, flushed as it happens:

```json
{"event":"progress","done":67108864,"total":251658240,"bytes_per_second":319800000,"connections":8}
{"event":"done","path":"out.iso","size":251658240,"sha256":"91c05498...","seconds":2,"connections":8,"mirrors":1,"duplicated":0}
```

Errors arrive the same way, as `{"event":"error","message":"..."}`, and the exit code is non-zero.
The fields are flat on purpose: a shell can read them with parameter expansion and no jq.
[examples/fetch.sh](examples/fetch.sh).

This is the right choice for most tools. It costs one process, it cannot take your program down
with it, and upgrading the engine does not mean rebuilding everything that uses it.

---

## 3. Over the network

For when the program that wants a file is not on the machine with the bandwidth: a laptop on
hotel wifi asking the box at home, or a fleet sharing one fat line.

```sh
# on the machine with the line
dn serve --listen 0.0.0.0:7878 --dir /srv/pool --token hunter2
```

```sh
# from anywhere that can reach it
curl -X POST -H "Authorization: Bearer hunter2" http://box:7878/download \
     -d '{"urls":["https://mirror/x.iso"],"name":"x.iso","expect":"91c05498...","streams":4}'
```

The reply is the same newline delimited JSON, streamed while it runs, so a client sees progress
without polling. `GET /health` needs no token.

Two rules keep this safe to run:

* it binds `127.0.0.1` unless told otherwise, and any other bind **requires** `--token`. An open
  downloader on a network is a way to fill someone's disk and to launder traffic through them.
* every file lands under `--dir`. A request picks a **name**, never a path, so `../../etc/passwd`
  becomes `passwd` inside the pool and nothing else.

Treat the token as a password, put it behind your own TLS or a tunnel if it crosses anything
public, and give the pool its own disk or quota.

---

## Choosing

Start with the **local process**: it is the least coupling for the most benefit, and it is how
`arxburn` (one of the programs that uses this) does it. Move to **compiled in** when you want the progress inside your own
window, or when starting a process per download is genuinely too much. Use **over the network**
when the bandwidth and the program are on different machines.

## Reading the source

If you want to understand the engine rather than use it, read it in this order:

1. `src/sched.rs` is the whole idea: which connection fetches which block, and when a slow one
   gets raced. The rules are pure functions over a snapshot, and each has a test that states an
   exact situation.
2. `src/lib.rs` is the loop that runs those rules: threads, the shared plan, and what happens to a
   block when an attempt comes back short, empty or late.
3. `src/transport.rs` is the only part that talks to a network, and it is deliberately small so
   another one can replace it.
4. `src/state.rs` and `src/probe.rs` are each a page, and both are about not trusting what you are
   told: a resume record that might describe a different build, a server that advertises ranges
   and then ignores them.
