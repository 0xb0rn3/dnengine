/* Using the engine from C.
 *
 *   cc examples/simple.c -Iinclude -Ltarget/release -ldnengine -o /tmp/dnc
 *   LD_LIBRARY_PATH=target/release /tmp/dnc https://host/file.iso /tmp/file.iso
 */
#include <stdio.h>
#include <string.h>
#include "dnengine.h"

static int on_progress(uint64_t done, uint64_t total, uint64_t bps, int conns, void *user)
{
    (void)user;
    if (total) {
        fprintf(stderr, "\r  %3llu%%  %llu / %llu bytes  %llu B/s  %d connections   ",
                (unsigned long long)(done * 100 / total), (unsigned long long)done,
                (unsigned long long)total, (unsigned long long)bps, conns);
    }
    return 0; /* non-zero would cancel */
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s <url> <dest> [sha256]\n", argv[0]);
        return 2;
    }
    const char *urls[1] = { argv[1] };
    dn_options opts;
    memset(&opts, 0, sizeof opts);          /* zeroed means: sensible defaults */
    opts.streams_per_lane = 4;
    if (argc > 3) opts.expect_sha256 = argv[3];

    printf("%s\n", dn_version());
    uint64_t size = 0;
    int ranges = dn_probe(argv[1], &size);
    printf("  server: %llu bytes, ranges %s\n", (unsigned long long)size, ranges == 1 ? "yes" : "no");

    int rc = dn_download(urls, 1, argv[2], &opts, on_progress, NULL);
    fprintf(stderr, "\n");
    if (rc != DN_OK) {
        fprintf(stderr, "  failed (%d): %s\n", rc, dn_last_error());
        return 1;
    }
    char hex[65];
    if (dn_sha256_file(argv[2], hex) == DN_OK) printf("  sha256 %s\n", hex);
    printf("  ok %s\n", argv[2]);
    return 0;
}
