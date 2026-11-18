#include <stdio.h>
#include "yesno.h"
/* A line from standard input: true if it starts with 'y' or 'Y'. */
int yesno(void) {
    int c, first;
    first = c = getchar();
    while (c != '\n' && c != EOF) c = getchar();
    return first == 'y' || first == 'Y';
}
