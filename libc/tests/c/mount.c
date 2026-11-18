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
    /* the cwd is a path string, `..` is lexical and crosses mount boundaries, openat resolves from a path-annotated descriptor */
    char cwd[256];
    CHECK(getcwd(cwd, sizeof cwd) != NULL && cwd[0] == '/', "getcwd returns an absolute path");
    CHECK(chdir(mnt) == 0, "chdir into the volume");
    CHECK(getcwd(cwd, sizeof cwd) != NULL && strcmp(cwd, mnt) == 0, "getcwd names it");
    CHECK(chdir("../../home") == 0, "chdir with .. climbs out of the volume to the root's /home");
    CHECK(getcwd(cwd, sizeof cwd) != NULL && strcmp(cwd, "/home") == 0, "and getcwd says so");
    int dfd = open("/home/sub", O_RDONLY | O_DIRECTORY);
    CHECK(dfd >= 0, "open a directory by path");
    int upfd = dfd >= 0 ? openat(dfd, "../hello.txt", O_RDONLY) : -1;
    CHECK(upfd >= 0, "openat with .. from a path-annotated directory descriptor");
    if (upfd >= 0) close(upfd);
    if (dfd >= 0) close(dfd);
    CHECK(chdir("/../../..") == 0 && getcwd(cwd, sizeof cwd) != NULL && strcmp(cwd, "/") == 0, "`..` above the root stays at the root");
    CHECK(open("", O_RDONLY) < 0 && errno == ENOENT, "the empty path names nothing");
    /* /dev and the private /tmp every program is given */
    int nul = open("/dev/null", O_WRONLY);
    CHECK(nul >= 0 && write(nul, "x", 1) == 1, "write to /dev/null");
    if (nul >= 0) close(nul);
    int zer = open("/dev/zero", O_RDONLY);
    char zb[8] = {1, 1, 1, 1, 1, 1, 1, 1};
    CHECK(zer >= 0 && read(zer, zb, 8) == 8 && zb[0] == 0 && zb[7] == 0, "read zeros from /dev/zero");
    if (zer >= 0) close(zer);
    FILE *tf = fopen("/tmp/scratch", "w");
    CHECK(tf != NULL && fputs("private", tf) >= 0 && fclose(tf) == 0, "write a file in the private /tmp");
    /* UV3: POSIX attributes are advisory data on the node; the defaults are synthesized */
    char lp[256], hp[256], dp[256];
    snprintf(path, sizeof path, "%s/posix-file", mnt);
    snprintf(lp, sizeof lp, "%s/posix-link", mnt);
    snprintf(hp, sizeof hp, "%s/posix-hard", mnt);
    snprintf(dp, sizeof dp, "%s/posix-dir", mnt);
    unlink(path); unlink(lp); rmdir(dp);
    f = fopen(path, "w");
    if (f) { fputs("abc", f); fclose(f); }
    CHECK(stat(path, &st) == 0 && S_ISREG(st.st_mode) && (st.st_mode & 0777) == 0644, "a new file has the default mode 0644");
    CHECK(st.st_nlink == 1 && st.st_size == 3 && st.st_mtime > 0, "one link, its size and a modification time");
    CHECK(chmod(path, 0755) == 0 && stat(path, &st) == 0 && (st.st_mode & 07777) == 0755, "chmod, then stat shows the mode");
    CHECK(access(path, X_OK) == 0, "access sees the execute bit");
    CHECK(chmod(path, 0600) == 0 && access(path, X_OK) != 0, "and sees it cleared");
    CHECK(chown(path, 42, 43) == 0 && stat(path, &st) == 0 && st.st_uid == 42 && st.st_gid == 43, "chown sets the advisory owner and group");
    int ffd = open(path, O_RDWR);
    CHECK(ffd >= 0 && fchmod(ffd, 0640) == 0 && fstat(ffd, &st) == 0 && (st.st_mode & 0777) == 0640, "fchmod and fstat on a descriptor");
    if (ffd >= 0) close(ffd);
    umask(077);
    CHECK(mkdir(dp, 0777) == 0 && stat(dp, &st) == 0 && S_ISDIR(st.st_mode) && (st.st_mode & 0777) == 0700, "mkdir applies the umask");
    umask(022);
    CHECK(symlink("posix-file", lp) == 0, "make a symlink");
    CHECK(lstat(lp, &st) == 0 && S_ISLNK(st.st_mode), "lstat describes the link itself");
    CHECK(stat(lp, &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 3, "stat follows it");
    char tb[64];
    long tn = readlink(lp, tb, sizeof tb);
    CHECK(tn == 10 && memcmp(tb, "posix-file", 10) == 0, "readlink gives the target text");
    char sb[64];
    CHECK(symlinkat("posix-file", AT_FDCWD, hp) == 0 && readlinkat(AT_FDCWD, hp, sb, sizeof sb) == 10 && memcmp(sb, "posix-file", 10) == 0, "symlinkat and readlinkat");
    unlink(hp);
    CHECK(symlink("/etc/passwd", hp) != 0 && errno == EINVAL, "an absolute link target is refused");
    CHECK(link(path, hp) != 0 && errno == EPERM, "hard links are refused");
    unlink(lp); unlink(path); rmdir(dp);
    /* leave one file behind: the shell test reads it from a later session (the volume is durable) */
    char kept[256];
    snprintf(kept, sizeof kept, "%s/kept", mnt);
    f = fopen(kept, "w");
    CHECK(f != NULL && fputs("kept across sessions\n", f) >= 0 && fclose(f) == 0, "leave a file for the next session");
    printf("mount: %d failures\n", failures);
    return failures;
}
