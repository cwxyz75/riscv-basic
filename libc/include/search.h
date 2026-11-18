#ifndef _OS_SEARCH_H
#define _OS_SEARCH_H
#include <stddef.h>
void insque(void *elem, void *pred);
void remque(void *elem);
void *lfind(const void *key, const void *base, size_t *nelp, size_t width, int (*cmp)(const void *, const void *));
void *lsearch(const void *key, void *base, size_t *nelp, size_t width, int (*cmp)(const void *, const void *));
typedef struct entry { char *key; void *data; } ENTRY;
typedef enum { FIND, ENTER } ACTION;
int hcreate(size_t nel);
void hdestroy(void);
ENTRY *hsearch(ENTRY item, ACTION action);
/* the tree is a plain binary search tree: `twalk` visits the same keys in the same in-order sequence as any other, but the
 * pre/post/end-order calls follow this tree's shape, not glibc's balanced one */
typedef enum { preorder, postorder, endorder, leaf } VISIT;
void *tsearch(const void *key, void **rootp, int (*cmp)(const void *, const void *));
void *tfind(const void *key, void *const *rootp, int (*cmp)(const void *, const void *));
void *tdelete(const void *key, void **rootp, int (*cmp)(const void *, const void *));
void twalk(const void *root, void (*action)(const void *, VISIT, int));
void tdestroy(void *root, void (*free_node)(void *));
#endif
