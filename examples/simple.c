/* cc -Iinclude examples/simple.c -Ltarget/release -ldnengine -o target/example-c
 * usage: example-c <url> <dest> [url dest ...]
 */
#include "dnengine.h"
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    if (argc < 3 || argc % 2 != 1) {
        fprintf(stderr, "usage: %s <url> <dest> [url dest ...]\n", argv[0]);
        return 2;
    }
    size_t n = (size_t)(argc - 1) / 2;
    const char **urls = calloc(n, sizeof *urls), **dests = calloc(n, sizeof *dests);
    dn_item_result *results = calloc(n, sizeof *results);
    if (!urls || !dests || !results) { free(urls); free(dests); free(results); return 1; }
    for (size_t i = 0; i < n; i++) { urls[i] = argv[1 + i*2]; dests[i] = argv[2 + i*2]; }
    dn_client_options opts;
    dn_client_options_init(&opts);
    opts.per_host = 2;
    dn_client *client = dn_client_new(&opts);
    int failed = 0;
    if (!client || dn_fetch_many_ex(client, urls, dests, n, results, NULL, NULL, NULL) != DN_OK) {
        fprintf(stderr, "%s\n", dn_last_error()); failed = 1;
    } else {
        for (size_t i = 0; i < n; i++) {
            printf("%zu: status=%d HTTP=%d bytes=%llu %s\n", i, results[i].status,
                   results[i].http_status, (unsigned long long)results[i].bytes, results[i].error);
            if (results[i].status != DN_OK) failed = 1;
        }
        /* The same client's connection pool also serves bounded in-memory requests. */
        uint8_t *data = NULL;
        size_t len = 0;
        if (dn_fetch_bytes(client, urls[0], 16*1024*1024, &data, &len, NULL, NULL) == DN_OK) {
            printf("memory: %zu bytes\n", len);
            dn_buffer_free(data, len);
        } else { fprintf(stderr, "%s\n", dn_last_error()); failed = 1; }
    }
    dn_client_free(client);
    free(urls); free(dests); free(results);
    return failed;
}
