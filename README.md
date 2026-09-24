# dnengine

A reusable HTTP(S) download library for Rust and any language with a C FFI. Fetch many
files through a persistent connection pool, receive bytes in memory or through a callback,
or split a large file across connections, mirrors and interfaces.

The default backend links system libcurl. There are zero Cargo dependencies, but builds
need a C compiler and libcurl development files. Deployment needs libcurl and a CA trust
store. The explicit Rust `transport::Curl` alternative still uses the curl executable.
See [BUILD.md](BUILD.md) and [TRANSPORT.md](TRANSPORT.md) for requirements and tradeoffs.

```rust
use dnengine::http::{Cancellation, Client, Options, Status};

let mut client = Client::new()?;
let opts = Options { per_host: 2, ..Default::default() };
let response = client.fetch_bytes(url, 16 * 1024 * 1024, &opts, &Cancellation::new())?;
if response.result.status != Status::Ok {
    return Err(response.result.error.into());
}
// Process response.body directly, for example with an archive reader.
```

The client has global and per-origin request caps, shared 429/503 cooldowns, Retry-After
support, bounded retries, cancellation, deadlines, and per-item results. Failed batch
items leave existing destination files intact. Keep a client alive to reuse its pool
between calls. Separate clients have independent limits.

| Workload | Rust | C ABI |
| --- | --- | --- |
| Many files | `Client::fetch_many` | `dn_fetch_many_ex`, or convenience `dn_fetch_many` |
| Bounded memory | `Client::fetch_bytes` | `dn_fetch_bytes`, `dn_buffer_free` |
| Incremental processing | `Client::fetch_stream` | `dn_fetch_stream` |
| One large file, ranges/mirrors/interfaces | `Download` with `transport::Native` | `dn_download` |

The existing range scheduler is derived from Plexo (MIT, attribution in `src/sched.rs`).
Parallel ranges can help with server connection limits or multiple routes. They do not
add bandwidth when the caller's own link is already saturated. Resume and hash checks
remain available for range downloads; use an expected hash when resuming.

See [INTEGRATION.md](INTEGRATION.md), [include/dnengine.h](include/dnengine.h), and
[examples](examples) for ownership, thread safety and language bindings. The Python and
Go examples pass many files to `dn_fetch_many`. Python also demonstrates persistent
clients and bounded memory buffers. The Go example is currently unverified on this host.

```sh
cargo build --release
cargo test --no-run
# After building the examples as described in BUILD.md:
python3 tests/vm.py
```

Runtime tests run only in a diskless VM without external networking. Results and measured
limits are in [VERIFICATION.md](VERIFICATION.md). This is not a claim of aria2/libcurl
feature parity: the API covers downloads, not all HTTP methods or authentication schemes.

```sh
dn get https://mirror.example/image.iso -o image.iso --expect "$SHA256"
dn get https://a.example/image.iso https://b.example/image.iso -o image.iso
dn get https://example/file -o file --single
dn probe https://example/file
dn serve --dir /srv/pool --token "$TOKEN"
```

A service listening outside loopback requires a token. Use TLS or a tunnel when exposing
it across a network and choose a storage quota appropriate to your application.
