/* dnengine: the ArxOS download engine, as a plain C library.
 *
 *   cc app.c -ldnengine -o app
 *
 * Many connections, every network interface, several mirrors at once, resumable, and verified
 * against a hash. Nothing in this header is Rust specific, so C, C++, Go (cgo), Python (ctypes),
 * Zig and anything else with a C FFI can use the same engine. Shell scripts use the `dn`
 * binary, which is this library with a command line on top.
 */
#ifndef DNENGINE_H
#define DNENGINE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define DN_OK            0
#define DN_ERR_ARGS      1
#define DN_ERR_NETWORK   2
#define DN_ERR_INTEGRITY 3
#define DN_ERR_PANIC     4

/* Return non-zero to cancel the download. */
typedef int (*dn_progress_fn)(uint64_t done, uint64_t total, uint64_t bytes_per_second,
                              int connections, void *user);

typedef struct {
    int streams_per_lane;        /* connections per mirror x interface; 0 means 2 */
    int max_connections;         /* total; 0 means 8 */
    const char *expect_sha256;   /* 64 hex characters, or NULL */
    const char *interfaces;      /* "wlan0,eth0", or NULL to work it out */
    int no_resume;               /* 0 continues an interrupted download */
} dn_options;

/* All urls must serve the SAME file; they are raced and combined. */
int dn_download(const char *const *urls, int url_count, const char *dest,
                const dn_options *opts, dn_progress_fn progress, void *user);

/* Human readable reason for the last failure. Never NULL. */
/* Fetch many files in one batch, reusing connections across all of them.
 *
 * This is the entry point for the workload dn_download cannot help with: lots of SMALL files.
 * Splitting a 2MB image into ranges buys nothing; what costs is per-file setup, and fetching
 * them one process at a time pays a spawn, a TCP handshake and a TLS negotiation for every
 * single one. A batch is one transfer process with one connection pool, so the socket and the
 * TLS session are reused and HTTP/2 multiplexes the requests.
 *
 * Measured at 4.4x against one-process-per-file for 120 files over TLS on loopback, where there
 * is no network latency to save; over a real link the gap is wider.
 *
 * urls and dests are parallel arrays of `count` NUL-terminated strings. `parallel` is the number
 * of concurrent transfers inside the process (0 means 8; above ~16 a single host stops going
 * faster and starts answering 429). progress may be NULL; returning non-zero from it stops the
 * batch. Returns how many succeeded, or -1 on a setup failure with dn_last_error() set. */
int dn_fetch_many(const char *const *urls, const char *const *dests, int count,
                  int parallel, dn_progress_fn progress, void *user);

const char *dn_last_error(void);

/* 1 when real byte ranges are available, 0 when not, -1 on a bad argument.
 * Writes the file size through size_out when that is not NULL. */
int dn_probe(const char *url, uint64_t *size_out);

/* Writes 64 hex characters and a NUL into out, which needs 65 bytes. */
int dn_sha256_file(const char *path, char *out);

const char *dn_version(void);

#ifdef __cplusplus
}
#endif
#endif /* DNENGINE_H */
