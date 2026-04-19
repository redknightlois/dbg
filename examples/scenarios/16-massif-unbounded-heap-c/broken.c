/* A request handler that caches parsed metadata keyed by tenant id
 * — but the cache never evicts, and we see RSS climb linearly with
 * request count in production. Tenants are effectively unbounded
 * (new ones every hour), so the cache is an unintentional memory
 * leak dressed up as a cache.
 *
 * Massif will show heap-xtree dominated by `add_to_cache` callers
 * and a monotonically growing total, which the team can then map
 * back to the responsible call site. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct Entry {
    int tenant;
    char *meta;  /* 4 KiB blob per tenant */
    struct Entry *next;
} Entry;

static Entry *cache = NULL;

static void add_to_cache(int tenant) {
    Entry *e = malloc(sizeof(Entry));
    e->tenant = tenant;
    e->meta = calloc(4096, 1);
    snprintf(e->meta, 4096, "tenant=%d/metadata=...", tenant);
    e->next = cache;
    cache = e;
}

static void handle_request(int tenant) {
    /* Real code would check-then-insert; omitted for clarity. */
    add_to_cache(tenant);
}

int main(void) {
    for (int i = 0; i < 4000; i++) {
        handle_request(i);
    }
    printf("processed 4000 requests\n");
    /* Intentionally no cleanup — matches long-lived server process. */
    return 0;
}
