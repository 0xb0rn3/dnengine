# Build and verify dnengine

Build on the host. Run all runtime tests in a throwaway diskless VM.

## Requirements

The crate has zero Cargo dependencies. The default HTTP backend links system libcurl,
so zero Cargo dependencies does not mean zero system dependencies.

- Rust 1.85 or newer, a C compiler, and `ar`.
- libcurl development headers and library, version 7.85 or newer, built with asynchronous
  DNS and thread-safe initialization. HTTPS needs a TLS-enabled build and a CA trust store.
- Linux/Unix is the current target. The range writer uses Unix file APIs. This session's
  verification targets Linux x86_64; other targets are not verified.
- `curl` on PATH only for the explicit legacy `transport::Curl` backend and its tests.
- VM verification: QEMU x86_64, a readable Linux kernel, cpio, gzip, OpenSSL, Python,
  iproute2, a shell, and the standard tools used by `tests/vm.py`.

System libcurl keeps HTTP, TLS, proxy handling and protocol updates with the operating
system. See [TRANSPORT.md](TRANSPORT.md) for the decision and compatibility changes.

## Host builds

```sh
cargo build --release
cargo test --no-run
cc -Wall -Wextra -Werror -Iinclude examples/simple.c \
  -Ltarget/release -ldnengine -o target/example-c
cargo build --manifest-path examples/rust-app/Cargo.toml --release \
  --target-dir target/example-build
cp target/example-build/release/rust-app target/example-rust
CGO_ENABLED=1 CGO_LDFLAGS="-L$PWD/target/release" \
  go build -o target/example-go examples/app.go
```

The Go compiler is optional for the engine build. If it is absent, the VM reports the
Go example as unverified. C, Python, Rust and shell checks remain required. A Go toolchain
was unavailable in the implementation session; do not describe that example as tested.

Outputs include `target/release/dn`, `libdnengine.so`, `libdnengine.a`, and the Rust rlib.
`build.rs` compiles `native/curl.c` with system headers, avoiding handwritten variadic
libcurl constants in Rust. `CC` and `AR` may be overridden. Cross builds also need the
correct target libcurl headers and libraries; they are not supplied by this repository.

For C with a static dnengine archive and dynamic system libraries:

```sh
cc -Iinclude examples/simple.c target/release/libdnengine.a \
  -lcurl -lpthread -ldl -lm -o target/example-c-static
```

This does not produce a fully static executable. Static libcurl needs its own transitive
link flags, as reported by the target's `pkg-config --static --libs libcurl`.

## Diskless runtime suite

```sh
python3 tests/vm.py
# Optional: keep the initramfs tree for inspection
python3 tests/vm.py --keep
```

Do not run `cargo test`, the examples, or `tests/bindings.py` directly on the host.
The harness performs `cargo test --no-run` on the host, copies the test binaries,
examples, Python modules and shared-library closure into an initramfs, then boots:

```text
qemu-system-x86_64 -nographic -no-reboot -m 1024 -smp 2 -nic none \
  -kernel /boot/vmlinuz-linux -initrd <temporary-initramfs> \
  -append "console=ttyS0 quiet panic=1"
```

It attaches no disks, NICs or host filesystem mounts. HTTP and TLS fixtures use only
loopback inside the guest. Test files live in guest RAM. A fresh test certificate and key
are generated for each initramfs and removed with it. `DN_VM_KERNEL` overrides the kernel.
The complete console output is saved to `target/vm-output.log`.

The suite covers the existing unit tests, origin admission and retry timing, active and
queued cancellation, stable item results, atomic batch output, memory limits, streaming,
redirects, native ranges, error-pointer isolation, TLS certificate checks, in-memory ZIP
processing, and the available language examples. It also compares native batching with
one pooled `curl --parallel` process over TLS HTTP/1.1. This is a loopback regression
fixture, not a measurement of internet throughput or HTTP/2 performance.

## Source map

| File | Responsibility |
| --- | --- |
| `src/http.rs` | Persistent clients, origin policy, redirects, retries, delivery and cancellation |
| `native/curl.c`, `build.rs` | Typed system libcurl ABI shim and host build |
| `src/batch.rs` | Existing batch API compatibility wrapper |
| `src/transport.rs` | Native range transport and explicit legacy subprocess transport |
| `src/lib.rs`, `src/sched.rs` | Range scheduling, progress, resume and hash verification |
| `src/probe.rs` | Cancellable header probes and final-response parsing |
| `src/ffi.rs`, `include/dnengine.h` | C ABI ownership, errors and concurrency contract |
| `tests/vm.py`, `tests/http.rs`, `tests/bindings.py` | Diskless verification and reproducible measurements |

Keep the MIT attribution in `src/sched.rs`. Use the repository's required author identity
for commits and omit co-author trailers.
