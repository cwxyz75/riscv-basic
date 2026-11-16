/* A volume granted at a mount point (FS-SPEC 5e): files made, read back, renamed and removed through an absolute path inside
 * the grant. Run on the OS only: `ctest mount /mnt/<name>`. Every check prints `ok: <what>` or `FAIL: <what>`; the return
 * value is the number of failures. */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/stat.h>

static int failures;

#define CHECK(cond, what) do { if (cond) printf("ok: %s\n", what); else { printf("FAIL: %s (errno %d)\n", what, errno); failures++; } fflush(stdout); } while (0)

int mount_test(const char *mnt) {
    char path[256], path2[256], buf[64];
    snprintf(path, sizeof path, "%s/made-by-ctest", mnt);
    snprintf(path2, sizeof path2, "%s/renamed", mnt);
    unlink(path);
    unlink(path2);
    FILE *f = fopen(path, "w");
    CHECK(f != NULL, "create a file in the volume");
    if (f) {
        CHECK(fputs("hello from a granted volume", f) >= 0, "write it");
        CHECK(fclose(f) == 0, "close it");
    }
    f = fopen(path, "r");
    CHECK(f != NULL, "open it again");
    if (f) {
        memset(buf, 0, sizeof buf);
        CHECK(fgets(buf, sizeof buf, f) != NULL && strcmp(buf, "hello from a granted volume") == 0, "read the same bytes");
        fclose(f);
    }
    struct stat st;
    CHECK(stat(path, &st) == 0 && st.st_size == 27, "stat reports its size");
    CHECK(rename(path, path2) == 0, "rename inside the volume");
    CHECK(stat(path, &st) != 0 && errno == ENOENT, "the old name is gone");
    CHECK(unlink(path2) == 0, "unlink it");
    char dir[256];
    snprintf(dir, sizeof dir, "%s/a-directory", mnt);
    rmdir(dir);
    CHECK(mkdir(dir, 0755) == 0, "make a directory");
    CHECK(rmdir(dir) == 0 || unlink(dir) == 0, "remove it");
    char up[256];
    snprintf(up, sizeof up, "%s/../escape", mnt);
    CHECK(open(up, O_RDONLY) < 0, "a path with .. does not leave the volume");
    /* leave one file behind: the shell test reads it from a later session (the volume is durable) */
    char kept[256];
    snprintf(kept, sizeof kept, "%s/kept", mnt);
    f = fopen(kept, "w");
    CHECK(f != NULL && fputs("kept across sessions\n", f) >= 0 && fclose(f) == 0, "leave a file for the next session");
    printf("mount: %d failures\n", failures);
    return failures;
}
