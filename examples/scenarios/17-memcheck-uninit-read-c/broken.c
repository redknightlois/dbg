/* A tiny config parser that reads key/value pairs into a Config
 * struct. Two separate bugs we want memcheck to catch:
 *   1) `flags` is never initialized — reads later use whatever was
 *      on the stack at call time. Non-deterministic behavior in
 *      production, hard to reproduce by running.
 *   2) When `value_buf` is NULL on exit, it's still freed — classic
 *      invalid free / use of uninit.
 *
 * Running the program will *usually* print something plausible
 * (stack garbage happens to be zero), but memcheck flags the read
 * on the first use of `flags`. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    int retries;
    int timeout_ms;
    int flags;     /* never initialized */
} Config;

static Config load(void) {
    Config c;
    c.retries = 3;
    c.timeout_ms = 500;
    /* forgot: c.flags = 0; */
    return c;
}

int main(void) {
    Config c = load();
    char *value_buf = NULL;  /* would be allocated if key was present */
    if (c.flags & 0x1) {
        printf("debug logging enabled\n");
    }
    printf("retries=%d timeout=%d flags=%d\n",
           c.retries, c.timeout_ms, c.flags);
    free(value_buf);  /* free(NULL) is defined — but if value_buf had been
                         left uninit, this would be a wild free. */
    return 0;
}
