/* dnengine: HTTP(S), batches, streaming and ranged downloads through a C ABI.
 * Link the shared library with -ldnengine; static users also need -lcurl and
 * the Rust native system libraries. See BUILD.md and INTEGRATION.md.
 */
#ifndef DNENGINE_H
#define DNENGINE_H
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif
#define DN_OK            0
#define DN_ERR_ARGS      1
#define DN_ERR_NETWORK   2
#define DN_ERR_INTEGRITY 3
#define DN_ERR_PANIC     4
#define DN_ERR_CANCELLED 5
#define DN_ERR_LIMIT     6
#define DN_ERR_IO        7
#define DN_ERR_BUSY      8

/* Callbacks run synchronously on the calling thread. Return nonzero to cancel.
 * No exception, panic or longjmp may cross a callback boundary. The engine
 * borrows all input pointers until the call returns, unless stated otherwise. */
typedef int (*dn_progress_fn)(uint64_t done, uint64_t total, uint64_t bytes_per_second,
                              int connections, void *user);
typedef int (*dn_write_fn)(const uint8_t *data, size_t len, void *user);

/* Existing ABI layout retained. These URLs are mirrors of the SAME file. */
typedef struct {
    int streams_per_lane;        /* 0 means 2 */
    int max_connections;         /* 0 means 8; native origin cap also applies */
    const char *expect_sha256;
    const char *interfaces;
    int no_resume;
} dn_options;
int dn_download(const char *const *urls, int url_count, const char *dest,
                const dn_options *opts, dn_progress_fn progress, void *user);

/* Existing batch ABI retained. HTTP(S) only. One in-process pool per call,
 * default per-origin cap 4. Returns success count, or -1 on setup failure.
 * Progress uses item counts and runs during transfers and retry waits.
 * Cancelled and unstarted items count as failed. Prefer _ex for details. */
int dn_fetch_many(const char *const *urls, const char *const *dests, int count,
                  int parallel, dn_progress_fn progress, void *user);

/* Thread-local error. Never NULL. Borrowed until the next error-setting call
 * ON THIS OS THREAD, or thread exit. Copy it before another call. Independent
 * threads cannot invalidate it. Go callers must pin the OS thread while reading.
 * Per-item result errors below are owned by the caller and have no TLS lifetime. */
const char *dn_last_error(void);
int dn_probe(const char *url, uint64_t *size_out);
int dn_sha256_file(const char *path, char *out); /* out needs 65 bytes */
const char *dn_version(void);

typedef struct dn_client dn_client;
typedef struct dn_cancel_token dn_cancel_token;

/* Initialize with dn_client_options_init, then override fields. NULL options
 * selects defaults. Values here are literal: retries=0 disables retries, timeout
 * and total_timeout 0 disable those deadlines. Init sets 8 parallel, 4 per host,
 * 2 retries, 500ms initial/30s maximum backoff, 20s connect, 60s attempt,
 * 300s total. Retry-After is never shortened to max_backoff. */
typedef struct {
    uint32_t struct_size;
    uint32_t parallel;
    uint32_t per_host;
    uint32_t retries;
    uint32_t backoff_ms;
    uint32_t max_backoff_ms;
    uint32_t connect_timeout_ms;
    uint32_t timeout_ms;
    uint32_t total_timeout_ms;
    const char *ca_file;         /* NULL uses system trust; copied by new() */
} dn_client_options;
void dn_client_options_init(dn_client_options *out);
/* NULL on failure, with dn_last_error set. Client retains its pool and origin
 * cooldowns between calls. Serialize calls on one client; overlap/reentry
 * returns DN_ERR_BUSY. Separate clients run concurrently with independent caps. */
dn_client *dn_client_new(const dn_client_options *opts);
void dn_client_free(dn_client *client); /* only after all calls have returned */

dn_cancel_token *dn_cancel_new(void);
/* Thread-safe, idempotent, one-shot. Can be called from another thread or from
 * a callback, but not from a signal handler. NULL means no external cancellation. */
void dn_cancel(const dn_cancel_token *cancel);
void dn_cancel_free(dn_cancel_token *cancel); /* only after users have returned */

typedef struct {
    uint64_t index;              /* original input index, independent of completion order */
    int status;                 /* DN_OK or DN_ERR_* */
    int http_status;            /* last HTTP response, or 0 */
    int transport_code;         /* libcurl CURLcode, or 0 */
    uint32_t attempts;          /* HTTP requests started, including redirects */
    uint64_t bytes;             /* bytes delivered on final attempt, possibly partial */
    char error[256];            /* owned UTF-8 message, NUL terminated, possibly truncated */
} dn_item_result;

/* Returns DN_OK when ALL result slots have been filled, including failures and
 * cancellations. Check each result.status. On an operation/setup error, results
 * are unspecified and dn_last_error describes the failure. Empty batch is valid.
 * Each success atomically replaces its destination; failures keep old files.
 * Destinations must be distinct files, including aliases through symlinks. */
int dn_fetch_many_ex(dn_client *client, const char *const *urls,
                     const char *const *dests, size_t count, dn_item_result *results,
                     const dn_cancel_token *cancel, dn_progress_fn progress, void *user);

/* No temporary file. max_bytes is a hard body limit, including chunked bodies.
 * Returns DN_OK and an owned buffer, or an error with *data=NULL and *len=0.
 * result is optional. Free successful buffers with the exact original pointer
 * and length, including empty buffers. Never use free() from the caller's CRT. */
int dn_fetch_bytes(dn_client *client, const char *url, size_t max_bytes,
                   uint8_t **data, size_t *len, dn_item_result *result,
                   const dn_cancel_token *cancel);
void dn_buffer_free(uint8_t *data, size_t len);

/* Borrowed chunks are valid only during write(). No replay after any bytes reach
 * the sink. On failure the sink may already contain partial data. Nonzero from
 * write() returns DN_ERR_CANCELLED; retries occur only before any delivery. */
int dn_fetch_stream(dn_client *client, const char *url, dn_write_fn write, void *user,
                    dn_item_result *result, const dn_cancel_token *cancel);
#ifdef __cplusplus
}
#endif
#endif
