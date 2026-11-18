#include <stdlib.h>
#include <string.h>
#include "savedir.h"
/* The names in a directory (not "." and ".."), each NUL-terminated, the list ended by an extra NUL. */
char *streamsavedir(DIR *dirp, int option) {
    size_t len = 0, cap = 256;
    char *out = malloc(cap);
    struct dirent *e;
    (void)option;
    if (!out) return 0;
    while ((e = readdir(dirp))) {
        size_t n = strlen(e->d_name) + 1;
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
        if (len + n + 1 > cap) {
            while (len + n + 1 > cap) cap *= 2;
            char *g = realloc(out, cap);
            if (!g) { free(out); return 0; }
            out = g;
        }
        memcpy(out + len, e->d_name, n);
        len += n;
    }
    out[len] = 0;
    return out;
}
