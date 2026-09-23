# Using dnengine in your project

dnengine is a download engine you import. It is not tied to any distribution, framework or
runtime: it builds to a C ABI shared library, a static library, a Rust crate and a command line
binary, and anything that can call C can call it.

The authoritative ABI contract is [include/dnengine.h](include/dnengine.h). The persistent
client API below is preferred for new integrations. The later language sketches retain the
convenience ABI; runnable examples are in [examples](examples).


## Persistent clients, per-item results and memory delivery

Create a client once and retain it across calls. A client owns its socket/TLS pool and an
origin policy domain. `dn_client_options_init` sets defaults; the options are copied by
`dn_client_new`. Do not zero this new struct as a substitute for initialization. A NULL
options pointer uses defaults. The old `dn_options` layout and zero defaults are unchanged.

```c
dn_client_options options;
dn_client_options_init(&options);
options.parallel = 8;
options.per_host = 2;
dn_client *client = dn_client_new(&options);
if (!client) { /* copy dn_last_error() here */ }

const char *urls[] = { "https://example/a.zip", "https://example/b.zip" };
const char *paths[] = { "a.zip", "b.zip" };
dn_item_result items[2];
int rc = dn_fetch_many_ex(client, urls, paths, 2, items, NULL, NULL, NULL);
/* DN_OK means the result array is complete. Check items[i].status for EACH item.
 * index is the original input index, never completion order. A cancelled item
 * has DN_ERR_CANCELLED, including work that never started. */
if (rc == DN_OK) {
    for (size_t i = 0; i < 2; ++i) {
        /* items[i].http_status, transport_code, attempts, bytes, error */
    }
}
dn_client_free(client);
```

The default caps are eight active requests total and four per origin (scheme, host and
port), including HTTP/2 streams. Both are per client, not per process. Redirect targets
pass through the same admission queue. A 429 or 503 sets an origin cooldown immediately
after its headers, so queued requests to that origin wait while other origins can proceed.
Requests already active may finish. Retry-After accepts seconds or an HTTP date. The
engine applies exponential backoff and jitter when needed, with two additional attempts
by default. A server delay longer than the operation deadline causes a timeout, never an
early retry. Full policy and compatibility details are in [TRANSPORT.md](TRANSPORT.md).

Each item has its own HTTP status, libcurl error code, attempt count, byte count and
owned error text. File output is staged in the destination directory and renamed only
on success. Failed or cancelled batch items retain the old destination. Destinations
must be distinct files, including aliases, and should not be written concurrently by
other calls. An operation-level error leaves the result array unspecified.

Memory delivery needs no temporary file:

```c
uint8_t *data = NULL;
size_t len = 0;
dn_item_result item;
int rc = dn_fetch_bytes(client, "https://example/wallpapers.zip", 64 * 1024 * 1024,
                       &data, &len, &item, NULL);
if (rc == DN_OK) {
    /* Pass data/len to your archive reader, then free with the original pair. */
    dn_buffer_free(data, len);
}
```

`max_bytes` is a hard limit even for chunked responses. Failure returns no buffer.
For incremental consumption, `dn_fetch_stream` invokes `dn_write_fn` with borrowed
chunks. Return zero after consuming a chunk; nonzero aborts. A stream may already have
received partial bytes when a later error occurs. The engine never retries after any
body bytes were delivered, so callbacks do not receive an implicit replay.

Use `dn_cancel_new` to create a one-shot token, pass it to the transfer, and call
`dn_cancel` from a different thread or callback. Free the token only after its users
return. Free clients and buffers with their matching dnengine function, never with an
unrelated allocator. Functions are synchronous; use your application's worker/executor
for asynchronous integration.

Python's maintained example exposes this directly:

```python
import io, zipfile
from dn import Client  # examples/dn.py on your Python module path

with Client(per_host=2) as client:
    data = client.bytes("https://example/wallpapers.zip", max_bytes=64 * 1024 * 1024)
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        image = archive.read("wallpaper.jpg")
```

Rust callers use `http::{Client, Options, Cancellation}`; see the compiled example in
[examples/rust-app](examples/rust-app). `Cancellation` is cloneable and thread-safe;
`Client` is Send but not Sync and its transfer methods require `&mut self`.

---

## 1. What you can actually do with it

**Pull one file as fast as the network allows.** A single HTTP connection is limited by one TCP
stream's window and one server's per-connection shaping. dnengine opens several connections, asks
each for a different byte range, and writes them into the same file at the right offsets. On a
4GB ISO over a link that idles at 30 MB/s on one stream, the difference is not subtle.

**Use every mirror at once.** Give it five URLs for the same file and it treats them as one
source: ranges are spread across all of them, a slow mirror gets less work, and a mirror that dies
mid-transfer does not fail the download, its ranges are reissued elsewhere.

**Use every network interface at once.** Wifi and ethernet and a tethered phone are separate paths
to the internet. dnengine can bind connections to each, so their bandwidth adds up rather than
competing.

**Fetch thousands of small files without paying for each one.** A different problem with a
different answer. Splitting a 200KB thumbnail into ranges is pointless; what costs is setup, and
naively that is one process, one TCP handshake and one TLS negotiation per file. `dn_fetch_many`
runs the whole batch through one connection pool, so sockets and TLS sessions are reused and
HTTP/2 can multiplex requests when supported by the linked libcurl and server. The previous
subprocess batch measured 4.4x against one process per file for 120 TLS loopback files. Its
0.8x plain HTTP loopback result measured the wrong thing for that motivation: there was no
TLS handshake to save. These are historical results, not native-backend speed claims.
See [VERIFICATION.md](VERIFICATION.md) for the current comparison against pooled curl.

**Resume.** An interrupted transfer continues from what is on disk instead of starting over. This
is on by default.

**Verify.** Give it an expected sha256 and it refuses to hand you a file that does not match. It
will also hash a file for you without downloading anything.

**Survive a stall.** A connection that stops producing bytes is abandoned and its work reissued,
rather than hanging until a timeout that may never come.

**Report progress and be cancelled.** A callback gets bytes done, total, current rate and how many
connections are live, and can stop the transfer by returning non-zero.

### What it does not do

Be honest with yourself about these before adopting it.

- **System libcurl is required.** The default path is now in process, with no curl executable
  needed. `transport::Curl` remains an explicit Rust alternative. See [TRANSPORT.md](TRANSPORT.md)
  for the deployment and compatibility changes.
- **No BitTorrent yet.** HTTP and HTTPS only.
- **Error strings are thread-local and borrowed.** Copy `dn_last_error()` on the same OS
  thread immediately after a failing call. It stays valid until that thread next updates
  its error or exits. New item result structs own their error text.
- **It is not a general HTTP client.** No custom headers, no POST, no cookies, no auth in the
  public API. It fetches HTTP(S) URLs to files, bounded memory buffers or callbacks.

---

## 2. Which call do you want

| Your situation | Call | Why |
|---|---|---|
| One large file, one URL | `dn_download` with 1 url | ranges across several connections |
| One large file, several mirrors | `dn_download` with N urls | mirrors raced and combined |
| Many files, any size | `dn_fetch_many_ex` | persistent pool and per-item results |
| ZIP or other bytes in memory | `dn_fetch_bytes` | bounded body, no temporary file |
| Incremental processing | `dn_fetch_stream` | borrowed chunks and backpressure |
| Many files, each huge | `dn_fetch_many`, then `dn_download` per file | batch is per-file, not per-range |
| How big is it before I commit | `dn_probe` | one cheap request |
| Is this file what it should be | `dn_sha256_file` | no download involved |

Rule of thumb: if splitting a single file across connections would help, use `dn_download`. If the
cost is the number of files rather than the size of any one of them, use `dn_fetch_many`.

---

## 3. The API

```c
#define DN_OK            0
#define DN_ERR_ARGS      1   /* you passed something wrong */
#define DN_ERR_NETWORK   2   /* transfer failed */
#define DN_ERR_INTEGRITY 3   /* sha256 did not match; the file is not yours to trust */
#define DN_ERR_PANIC     4   /* Rust panic caught at the boundary */
#define DN_ERR_CANCELLED 5
#define DN_ERR_LIMIT     6
#define DN_ERR_IO        7
#define DN_ERR_BUSY      8

typedef int (*dn_progress_fn)(uint64_t done, uint64_t total, uint64_t bytes_per_second,
                              int connections, void *user);

typedef struct {
    int streams_per_lane;        /* connections per mirror x interface; 0 means 2 */
    int max_connections;         /* total ceiling; 0 means 8 */
    const char *expect_sha256;   /* 64 hex chars, or NULL to skip the check */
    const char *interfaces;      /* "wlan0,eth0", or NULL to work it out */
    int no_resume;               /* 0 continues an interrupted download */
} dn_options;

int  dn_download(const char *const *urls, int url_count, const char *dest,
                 const dn_options *opts, dn_progress_fn progress, void *user);
int  dn_fetch_many(const char *const *urls, const char *const *dests, int count,
                   int parallel, dn_progress_fn progress, void *user);
int  dn_probe(const char *url, uint64_t *size_out);
int  dn_sha256_file(const char *path, char *out);   /* out needs 65 bytes */
const char *dn_last_error(void);
const char *dn_version(void);
```

### Rules that matter

**Ownership.** dnengine never takes ownership of anything you pass and never retains a pointer
past the call that received it. You free your own strings whenever you like after the call
returns. The `const char *` returned by `dn_last_error()` and `dn_version()` is owned by the
library: do not free it, and copy it if you need it later.

**Buffers.** `dn_sha256_file` writes 64 hex characters plus a NUL, so `out` must have room for
**65 bytes**. Anything less is your bug and dnengine cannot detect it.

**The callback.** Return `0` to continue, **non-zero to cancel**. It runs synchronously on the
thread making the call. Keep it cheap. A callback may cancel its token; a transfer reentering
the same client returns `DN_ERR_BUSY`. Do not free a client or token while a call uses it.

**The `user` pointer** is opaque and handed back to your callback untouched. Use it for context
instead of globals.

**Thread safety.** Independent downloads and clients may be used from different threads with
distinct destinations and callback state. Calls on the same client must be serialized; an
overlap returns `DN_ERR_BUSY`. Each client has its own pool, limits and cooldowns. There is no
process-wide quota. `dn_last_error()` is OS-thread-local. Go code must use `runtime.LockOSThread`
across the failing call and the error read, or rely on the new owned item results.

**Panics.** Fallible transfer entry points catch Rust unwinding and return an error. Invalid C
pointers, allocator aborts and exceptions escaping foreign callbacks are outside that guarantee.
Never throw, panic or longjmp through a callback boundary.

**Cancellation is cooperative.** Native network waits poll at most 50ms at a time and remove
active transfers when cancelled. Callback execution and file I/O can take longer. Cancellation
also interrupts origin cooldowns. It is not a signal-handler API or a hard real-time guarantee.
The original download callback runs before probes and during range delivery; external Rust
cancellation also interrupts probes.

---

## 4. Building it

```sh
git clone https://github.com/0xb0rn3/dnengine && cd dnengine
cargo build --release
```

You get four things in `target/release/`:

| Artifact | Use it when |
|---|---|
| `libdnengine.so` | dynamic linking; ctypes, cgo, node-ffi, dlopen |
| `libdnengine.a` | static linking; one binary with no .so to ship |
| `dn` | the command line tool; shell scripts, Makefiles, CI |
| rlib | `dnengine = { path = "..." }` in a Rust project |

Runtime requirements: **system libcurl >= 7.85 with async DNS and thread-safe initialization,
and a CA trust store for HTTPS**. Building needs a C compiler and libcurl headers. See [BUILD.md](BUILD.md).

---

## 5. Per language

The files in [examples](examples) are the maintained examples. Additional language sketches
below illustrate the existing convenience ABI and are not all covered by the VM suite.

### C

```c
#include <stdio.h>
#include "dnengine.h"

static int on_progress(uint64_t done, uint64_t total, uint64_t bps, int conns, void *user) {
    if (total) fprintf(stderr, "\r%3d%%  %d connections  %.1f MB/s",
                       (int)(done * 100 / total), conns, bps / 1e6);
    return 0;                      /* non-zero here would cancel */
}

int main(void) {
    const char *mirrors[] = {
        "https://a.example/image.iso",
        "https://b.example/image.iso",
    };
    dn_options o = {0};            /* zeroed struct = sensible defaults */
    o.max_connections = 8;
    o.expect_sha256   = "18d87568b4e2cf4d2e83bb0052c93a5d76b12708bd942e86029ea612c1f2d44d";

    int rc = dn_download(mirrors, 2, "image.iso", &o, on_progress, NULL);
    if (rc != DN_OK) {
        fprintf(stderr, "\nfailed (%d): %s\n", rc, dn_last_error());
        return 1;
    }
    puts("\nverified");
    return 0;
}
```

```sh
# dynamic
cc app.c -Iinclude -Ltarget/release -ldnengine -o app
LD_LIBRARY_PATH=target/release ./app

# static: no .so to ship, but you must name what Rust's std needs
cc app.c -Iinclude target/release/libdnengine.a -lcurl -lpthread -ldl -lm -o app
```

### Rust

As a normal dependency, no FFI involved:

```toml
[dependencies]
dnengine = { git = "https://github.com/0xb0rn3/dnengine" }
```

```rust
use dnengine::{batch, Download, transport::Native};

fn main() -> Result<(), String> {
    // one file, several mirrors
    let urls = vec!["https://a.example/image.iso".to_string(),
                    "https://b.example/image.iso".to_string()];
    Download::new(urls, "image.iso").streams_per_lane(4)
        .run(&Native::default(), |p| eprint!("\r{} bytes", p.done))?;

    // many files, one pool
    let items: Vec<batch::Item> = (1..=500).map(|i| batch::Item {
        url:  format!("https://example/img/{i}.jpg"),
        dest: format!("out/{i}.jpg").into(),
    }).collect();
    let r = batch::fetch_many(&items, &batch::BatchOpts::default(), |done, total| {
        eprint!("\r{done}/{total}");
        true                        // false here would stop the batch
    })?;
    eprintln!("\n{} ok, {} failed", r.ok, r.failed);
    Ok(())
}
```

### Go (cgo)

The maintained [examples/app.go](examples/app.go) uses `dn_fetch_many` for URL/destination
pairs, C-owned arrays, and OS-thread pinning for error retrieval. Build instructions are in
[BUILD.md](BUILD.md). It is unverified in this session because no Go compiler was available.
The following is an alternative sketch for one ranged file.

```go
package main

/*
#cgo CFLAGS:  -I${SRCDIR}/include
#cgo LDFLAGS: -L${SRCDIR}/target/release -ldnengine
#include <stdlib.h>
#include "dnengine.h"

extern int goProgress(unsigned long long, unsigned long long, unsigned long long, int, void*);
static int dn_go(const char **u, int n, const char *dest) {
    dn_options o = {0};
    o.max_connections = 8;
    return dn_download(u, n, dest, &o, (dn_progress_fn)goProgress, NULL);
}
*/
import "C"
import (
	"fmt"
	"runtime"
	"unsafe"
)

//export goProgress
func goProgress(done, total, bps C.ulonglong, conns C.int, _ unsafe.Pointer) C.int {
	if total > 0 {
		fmt.Printf("\r%d%%", done*100/total)
	}
	return 0 // non-zero cancels
}

func main() {
	urls := []string{"https://a.example/image.iso", "https://b.example/image.iso"}

	// C array of C strings, freed by us: dnengine never retains them
	c := C.malloc(C.size_t(len(urls)) * C.size_t(unsafe.Sizeof(uintptr(0))))
	defer C.free(c)
	slice := (*[1 << 16]*C.char)(c)[:len(urls):len(urls)]
	for i, u := range urls {
		slice[i] = C.CString(u)
		defer C.free(unsafe.Pointer(slice[i]))
	}
	dest := C.CString("image.iso")
	defer C.free(unsafe.Pointer(dest))

	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	if rc := C.dn_go((**C.char)(c), C.int(len(urls)), dest); rc != 0 {
		fmt.Println("\nfailed:", C.GoString(C.dn_last_error()))
		return
	}
	fmt.Println("\ndone")
}
```

```sh
CGO_ENABLED=1 go build -o app . && LD_LIBRARY_PATH=target/release ./app
```

Note the shim: cgo cannot pass a Go func pointer as a C callback directly, so a tiny C function
takes the `//export`ed symbol and casts it.

### Python (ctypes, no build step)

```python
import ctypes, ctypes.util

dn = ctypes.CDLL("./target/release/libdnengine.so")

DN_OK = 0
PROGRESS = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_uint64, ctypes.c_uint64,
                            ctypes.c_uint64, ctypes.c_int, ctypes.c_void_p)

class Options(ctypes.Structure):
    _fields_ = [("streams_per_lane", ctypes.c_int),
                ("max_connections",  ctypes.c_int),
                ("expect_sha256",    ctypes.c_char_p),
                ("interfaces",       ctypes.c_char_p),
                ("no_resume",        ctypes.c_int)]

dn.dn_download.argtypes = [ctypes.POINTER(ctypes.c_char_p), ctypes.c_int, ctypes.c_char_p,
                           ctypes.POINTER(Options), PROGRESS, ctypes.c_void_p]
dn.dn_download.restype = ctypes.c_int
dn.dn_fetch_many.argtypes = [ctypes.POINTER(ctypes.c_char_p), ctypes.POINTER(ctypes.c_char_p),
                             ctypes.c_int, ctypes.c_int, PROGRESS, ctypes.c_void_p]
dn.dn_fetch_many.restype = ctypes.c_int
dn.dn_last_error.restype = ctypes.c_char_p
dn.dn_version.restype = ctypes.c_char_p
dn.dn_sha256_file.argtypes = [ctypes.c_char_p, ctypes.c_char_p]

@PROGRESS
def on_progress(done, total, bps, conns, user):
    if total:
        print(f"\r{done * 100 // total}%  {conns} conns  {bps/1e6:.1f} MB/s", end="")
    return 0                                  # non-zero cancels

def download(urls, dest, sha=None):
    arr = (ctypes.c_char_p * len(urls))(*[u.encode() for u in urls])
    o = Options(0, 8, sha.encode() if sha else None, None, 0)
    rc = dn.dn_download(arr, len(urls), dest.encode(), ctypes.byref(o), on_progress, None)
    if rc != DN_OK:
        raise RuntimeError(f"dnengine {rc}: {dn.dn_last_error().decode()}")

def fetch_many(pairs, parallel=8):
    """pairs: [(url, dest), ...] -> number that succeeded"""
    urls  = (ctypes.c_char_p * len(pairs))(*[u.encode() for u, _ in pairs])
    dests = (ctypes.c_char_p * len(pairs))(*[d.encode() for _, d in pairs])
    return dn.dn_fetch_many(urls, dests, len(pairs), parallel, on_progress, None)

def sha256(path):
    buf = ctypes.create_string_buffer(65)     # 64 hex + NUL
    if dn.dn_sha256_file(path.encode(), buf) != DN_OK:
        raise RuntimeError(dn.dn_last_error().decode())
    return buf.value.decode()

if __name__ == "__main__":
    print("dnengine", dn.dn_version().decode())
    download(["https://a.example/image.iso"], "image.iso")
    print(sha256("image.iso"))
```

Keep a reference to the `@PROGRESS` object alive for the whole call. If Python garbage collects
it, the library calls a freed function pointer.

### JavaScript and TypeScript (Node)

Two routes. Pick by whether you want a build step.

**Route A, no build step: drive the `dn` binary.** Robust, portable, works in any runtime that can
spawn a process, including Deno and Bun.

```js
import { spawn } from "node:child_process";

export function download(urls, dest, { onProgress } = {}) {
  return new Promise((resolve, reject) => {
    const p = spawn("./target/release/dn", ["get", ...urls, "-o", dest, "--json"]);
    let err = "";
    p.stdout.on("data", chunk => {
      for (const line of String(chunk).split("\n")) {
        if (!line.startsWith("{")) continue;
        const m = JSON.parse(line);
        if (m.event === "progress" && onProgress) onProgress(m);
      }
    });
    p.stderr.on("data", d => { err += d; });
    p.on("close", code => code === 0 ? resolve() : reject(new Error(err.trim() || `dn exited ${code}`)));
  });
}

await download(["https://a.example/image.iso"], "image.iso", {
  onProgress: m => process.stderr.write(`\r${Math.floor(m.done * 100 / m.total)}%`),
});
```

**Route B, in-process: call the shared library through koffi.**

```js
import koffi from "koffi";

const lib = koffi.load("./target/release/libdnengine.so");

const dn_options = koffi.struct("dn_options", {
  streams_per_lane: "int",
  max_connections:  "int",
  expect_sha256:    "const char *",
  interfaces:       "const char *",
  no_resume:        "int",
});
const dn_progress_fn = koffi.proto(
  "int dn_progress_fn(uint64_t done, uint64_t total, uint64_t bps, int conns, void *user)");

const dn_download    = lib.func("int dn_download(const char **urls, int n, const char *dest, const dn_options *o, dn_progress_fn *cb, void *user)");
const dn_last_error  = lib.func("const char *dn_last_error()");
const dn_version     = lib.func("const char *dn_version()");

const cb = koffi.register((done, total) => {
  if (total) process.stderr.write(`\r${Number(done * 100n / total)}%`);
  return 0;                                   // non-zero cancels
}, koffi.pointer(dn_progress_fn));

try {
  console.log("dnengine", dn_version());
  const rc = dn_download(["https://a.example/image.iso"], 1, "image.iso",
                         { streams_per_lane: 0, max_connections: 8,
                           expect_sha256: null, interfaces: null, no_resume: 0 },
                         cb, null);
  if (rc !== 0) throw new Error(`${rc}: ${dn_last_error()}`);
} finally {
  koffi.unregister(cb);                       // or the callback leaks
}
```

The callback runs on the thread making the synchronous call. Follow your FFI binding's
callback rules; move blocking transfers off the JavaScript event loop when needed.

### Shell

```sh
dn get https://a.example/image.iso https://b.example/image.iso \
   -o image.iso \
   --expect 18d87568b4e2cf4d2e83bb0052c93a5d76b12708bd942e86029ea612c1f2d44d \
   --max 8

dn probe https://a.example/image.iso     # what the server allows, before committing
dn net                                   # the ways onto the internet this machine has

# machine readable, one JSON object per line, for a wrapper in any language
dn get https://a.example/image.iso -o image.iso --json |
  while read -r line; do
    case "$line" in *'"event":"progress"'*) printf '.' ;; esac
  done
```

Exit status is the same code the C API returns, so `$?` distinguishes a network failure (2) from
an integrity failure (3).

### C++

The header is already `extern "C"` guarded, so it includes directly. This wraps it in RAII so a
cancelled or failed download cannot leak the callback context.

```cpp
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>
#include "dnengine.h"

class Download {
public:
    struct Ctx { double last = -1; };

    static void run(const std::vector<std::string>& urls, const std::string& dest,
                    const std::string& sha = {}) {
        std::vector<const char*> c;
        c.reserve(urls.size());
        for (const auto& u : urls) c.push_back(u.c_str());

        dn_options o{};                       // value-initialised: all defaults
        o.max_connections = 8;
        if (!sha.empty()) o.expect_sha256 = sha.c_str();

        Ctx ctx;
        int rc = dn_download(c.data(), static_cast<int>(c.size()), dest.c_str(), &o,
                             &Download::progress, &ctx);
        if (rc != DN_OK)
            throw std::runtime_error(std::string("dnengine: ") + dn_last_error());
    }

private:
    // must be a plain function or a capture-less lambda: a C callback has no `this`
    static int progress(uint64_t done, uint64_t total, uint64_t bps, int conns, void* user) {
        auto* ctx = static_cast<Ctx*>(user);
        if (!total) return 0;
        double pct = static_cast<double>(done) * 100.0 / static_cast<double>(total);
        if (pct - ctx->last >= 1.0) {         // throttle: this fires often
            ctx->last = pct;
            std::cerr << "\r" << static_cast<int>(pct) << "%  " << conns << " conns"
                      << "  " << bps / 1e6 << " MB/s" << std::flush;
        }
        return 0;                             // non-zero cancels
    }
};

int main() {
    try {
        Download::run({"https://a.example/image.iso", "https://b.example/image.iso"},
                      "image.iso");
        std::cout << "\nverified\n";
    } catch (const std::exception& e) {
        std::cerr << "\n" << e.what() << "\n";
        return 1;
    }
}
```

```sh
c++ -std=c++17 app.cpp -Iinclude -Ltarget/release -ldnengine -o app
LD_LIBRARY_PATH=target/release ./app
```

Do not throw out of the callback. It unwinds through C, which is undefined: catch inside it, set a
flag, and return non-zero to cancel.

### Java (JNA, no JNI to write)

```java
import com.sun.jna.*;
import java.util.List;

public interface Dn extends Library {
    Dn I = Native.load("dnengine", Dn.class);   // finds libdnengine.so on jna.library.path

    class Options extends Structure {
        public int streamsPerLane;
        public int maxConnections;
        public String expectSha256;
        public String interfaces;
        public int noResume;
        protected List<String> getFieldOrder() {
            return List.of("streamsPerLane", "maxConnections", "expectSha256",
                           "interfaces", "noResume");
        }
    }

    interface Progress extends Callback {
        int invoke(long done, long total, long bps, int conns, Pointer user);
    }

    int dn_download(String[] urls, int count, String dest, Options o, Progress cb, Pointer user);
    int dn_fetch_many(String[] urls, String[] dests, int count, int parallel,
                      Progress cb, Pointer user);
    String dn_last_error();
    String dn_version();

    static void main(String[] args) {
        System.out.println("dnengine " + I.dn_version());

        Options o = new Options();
        o.maxConnections = 8;

        // keep a strong reference: if this is collected, the native side calls freed memory
        Progress cb = (done, total, bps, conns, user) -> {
            if (total > 0) System.err.printf("\r%d%%", done * 100 / total);
            return 0;                            // non-zero cancels
        };

        int rc = I.dn_download(new String[]{"https://a.example/image.iso"}, 1,
                              "image.iso", o, cb, null);
        if (rc != 0) throw new RuntimeException(I.dn_last_error());
        System.out.println("\ndone");
    }
}
```

```sh
java -cp jna-5.14.0.jar:. -Djna.library.path=target/release Dn
```

### C# and .NET

```csharp
using System;
using System.Runtime.InteropServices;

static class Dn {
    const string LIB = "dnengine";               // libdnengine.so / dnengine.dll

    [StructLayout(LayoutKind.Sequential)]
    public struct Options {
        public int StreamsPerLane;
        public int MaxConnections;
        [MarshalAs(UnmanagedType.LPStr)] public string ExpectSha256;
        [MarshalAs(UnmanagedType.LPStr)] public string Interfaces;
        public int NoResume;
    }

    public delegate int Progress(ulong done, ulong total, ulong bps, int conns, IntPtr user);

    [DllImport(LIB)] public static extern int dn_download(
        string[] urls, int count, string dest, ref Options o, Progress cb, IntPtr user);
    [DllImport(LIB)] public static extern int dn_fetch_many(
        string[] urls, string[] dests, int count, int parallel, Progress cb, IntPtr user);
    [DllImport(LIB)] static extern IntPtr dn_last_error();
    [DllImport(LIB)] static extern IntPtr dn_version();

    public static string LastError() => Marshal.PtrToStringAnsi(dn_last_error());
    public static string Version()   => Marshal.PtrToStringAnsi(dn_version());

    static int Main() {
        Console.WriteLine($"dnengine {Version()}");
        var o = new Options { MaxConnections = 8 };

        // hold the delegate in a field or local that outlives the call, or the GC
        // collects it while native code still holds the pointer
        Progress cb = (done, total, bps, conns, user) => {
            if (total > 0) Console.Error.Write($"\r{done * 100 / total}%");
            return 0;                             // non-zero cancels
        };

        int rc = dn_download(new[]{"https://a.example/image.iso"}, 1, "image.iso",
                             ref o, cb, IntPtr.Zero);
        GC.KeepAlive(cb);
        if (rc != 0) { Console.Error.WriteLine($"\n{LastError()}"); return 1; }
        Console.WriteLine("\ndone");
        return 0;
    }
}
```

```sh
dotnet build && LD_LIBRARY_PATH=target/release dotnet run
```

### Ruby (Fiddle, in the standard library)

```ruby
require "fiddle"
require "fiddle/import"

module Dn
  extend Fiddle::Importer
  dlload "./target/release/libdnengine.so"

  extern "int dn_download(const char**, int, const char*, void*, void*, void*)"
  extern "int dn_fetch_many(const char**, const char**, int, int, void*, void*)"
  extern "const char* dn_last_error()"
  extern "const char* dn_version()"
  extern "int dn_sha256_file(const char*, char*)"
end

# dn_options laid out by hand: two ints, two pointers, one int
def options(max_connections: 8, expect_sha256: nil)
  sha = expect_sha256 ? Fiddle::Pointer[expect_sha256 + "\0"] : Fiddle::NULL
  [0, max_connections, sha.to_i, 0, 0].pack("l l Q Q l")
end

def urls_array(urls)
  ptrs = urls.map { |u| Fiddle::Pointer[u + "\0"].to_i }
  Fiddle::Pointer[ptrs.pack("Q*")]
end

PROGRESS = Fiddle::Closure::BlockCaller.new(
  Fiddle::TYPE_INT,
  [Fiddle::TYPE_LONG_LONG, Fiddle::TYPE_LONG_LONG, Fiddle::TYPE_LONG_LONG,
   Fiddle::TYPE_INT, Fiddle::TYPE_VOIDP]
) do |done, total, _bps, _conns, _user|
  $stderr.print "\r#{total > 0 ? done * 100 / total : 0}%"
  0                                              # non-zero cancels
end

puts "dnengine #{Dn.dn_version.to_s}"
opts = options(expect_sha256: nil)
rc = Dn.dn_download(urls_array(["https://a.example/image.iso"]), 1, "image.iso",
                    Fiddle::Pointer[opts], PROGRESS, Fiddle::NULL)
raise Dn.dn_last_error.to_s unless rc.zero?
puts "\ndone"
```

### PHP (FFI, PHP 7.4+)

```php
<?php
$dn = FFI::cdef(<<<'C'
    typedef int (*dn_progress_fn)(uint64_t, uint64_t, uint64_t, int, void *);
    typedef struct {
        int streams_per_lane;
        int max_connections;
        const char *expect_sha256;
        const char *interfaces;
        int no_resume;
    } dn_options;
    int dn_download(const char *const *urls, int url_count, const char *dest,
                    const dn_options *opts, dn_progress_fn progress, void *user);
    int dn_fetch_many(const char *const *urls, const char *const *dests, int count,
                      int parallel, dn_progress_fn progress, void *user);
    const char *dn_last_error(void);
    const char *dn_version(void);
C, "./target/release/libdnengine.so");

echo "dnengine " . $dn->dn_version() . "
";

$urls  = ["https://a.example/image.iso"];
$array = FFI::new("char*[" . count($urls) . "]");
$keep  = [];                                    // keep the strings alive for the call
foreach ($urls as $i => $u) {
    $s = FFI::new("char[" . (strlen($u) + 1) . "]", false);
    FFI::memcpy($s, $u, strlen($u));
    $keep[] = $s;
    $array[$i] = FFI::cast("char*", $s);
}

$opts = $dn->new("dn_options");
$opts->max_connections = 8;

$progress = function ($done, $total, $bps, $conns, $user) {
    if ($total > 0) fwrite(STDERR, sprintf("
%d%%", intdiv($done * 100, $total)));
    return 0;                                   // non-zero cancels
};

$rc = $dn->dn_download($array, count($urls), "image.iso", FFI::addr($opts), $progress, null);
foreach ($keep as $s) FFI::free($s);
if ($rc !== 0) { fwrite(STDERR, "
" . $dn->dn_last_error() . "
"); exit(1); }
echo "
done
";
```

### Lua (LuaJIT)

```lua
local ffi = require("ffi")

ffi.cdef[[
typedef int (*dn_progress_fn)(uint64_t, uint64_t, uint64_t, int, void *);
typedef struct {
    int streams_per_lane;
    int max_connections;
    const char *expect_sha256;
    const char *interfaces;
    int no_resume;
} dn_options;
int dn_download(const char *const *urls, int url_count, const char *dest,
                const dn_options *opts, dn_progress_fn progress, void *user);
int dn_fetch_many(const char *const *urls, const char *const *dests, int count,
                  int parallel, dn_progress_fn progress, void *user);
const char *dn_last_error(void);
const char *dn_version(void);
]]

local dn = ffi.load("./target/release/libdnengine.so")
print("dnengine", ffi.string(dn.dn_version()))

local urls  = {"https://a.example/image.iso"}
local array = ffi.new("const char *[?]", #urls)
for i, u in ipairs(urls) do array[i - 1] = u end

local opts = ffi.new("dn_options", { max_connections = 8 })

-- keep the callback in a local: if it is collected, the C side calls freed memory
local cb = ffi.cast("dn_progress_fn", function(done, total, bps, conns, user)
    if total > 0 then io.stderr:write(("
%d%%"):format(tonumber(done * 100 / total))) end
    return 0                                    -- non-zero cancels
end)

local rc = dn.dn_download(array, #urls, "image.iso", opts, cb, nil)
cb:free()
if rc ~= 0 then error(ffi.string(dn.dn_last_error())) end
print("\ndone")
```

### Anything else

If your language can `dlopen` a shared library and describe a C function, it can use dnengine
directly, and the six bindings above are the pattern: declare the struct, declare the functions,
keep the callback alive for the duration of the call. Zig (`@cImport`), Nim (`dynlib`), Crystal
(`@[Link]`), Perl (`FFI::Platypus`), Julia (`ccall`), Swift (module map), Haskell (`foreign
import ccall`) and Java 21's Panama all work this way. The contract is section 3 and nothing
about it is language specific.

If your language cannot do FFI, or you would rather not, spawn `dn get ... --json` and read
stdout. That is what the JavaScript Route A above does, it works from any language with
subprocesses, and it costs you one process per download rather than one per file.

**The one mistake every binding makes.** The callback must stay alive and reachable for the whole
call. Python needs a reference to the `@PROGRESS` object, C# needs `GC.KeepAlive`, Java needs a
strong reference, Lua needs the local plus an explicit `:free()`, koffi needs `unregister`. If
your garbage collector reclaims it while a download is running, the engine calls freed memory and
your process dies somewhere that looks nothing like the real cause.

---

## 6. Deployment checklist

- System libcurl and its TLS trust store are present on the target machine.
- `libdnengine.so` is where your loader will find it (`LD_LIBRARY_PATH`, `rpath`, or next to the
  binary), or you linked `libdnengine.a` statically and do not care.
- Your callbacks stay alive for the call, run cheaply, and do not reenter the same client.
- You check return codes. `DN_ERR_INTEGRITY` in particular means **do not use that file**.
- Copy `dn_last_error()` on the same OS thread as the failure, or use the owned item results.
