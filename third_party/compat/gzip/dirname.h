#ifndef DIRNAME_H
#define DIRNAME_H
#include <stddef.h>
static inline char *last_component(const char *name) {
    const char *base = name;
    int last_was_slash = 0;
    while (*name == '/') name++;
    base = name;
    for (; *name; name++) {
        if (*name == '/') last_was_slash = 1;
        else if (last_was_slash) { base = name; last_was_slash = 0; }
    }
    return (char *)base;
}
#endif
