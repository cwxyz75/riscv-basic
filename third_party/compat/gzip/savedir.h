#ifndef SAVEDIR_H
#define SAVEDIR_H
#include <dirent.h>
enum { SAVEDIR_SORT_NONE = 0 };
char *streamsavedir(DIR *dirp, int option);
#endif
