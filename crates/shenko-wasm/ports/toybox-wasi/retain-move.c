// retain-move — archive generated build targets without deleting them.
//
// Replaces the port's explicit upstream `rm` calls. Every argument is a path
// inside the run's vendor/, applets/ or scratch/ tree. Existing targets are
// moved atomically into <run>/retired/item.*/payload via dirfd-relative
// rename(RENAME_NOREPLACE): no pathname ancestor is re-resolved after
// validation, cross-device moves are rejected instead of copy+unlink, and an
// existing source is always preserved — never copied-as-delete, never
// silently dropped when the receipt cannot be written.
//
// Usage: retain-move [-f] [-r] [--] <path>...
// The owning build run comes from SHENKO_TOYBOX_RUN_DIR (bytes preserved —
// unlike command substitution, exec env never mangles trailing newlines).
//   -f   record absent targets instead of failing on them (like rm -f)
//   -r   allow directory targets (like rm -r)
//
// Receipt format in <run>/retired/INDEX.tsv:
//   pending\t<rel>\t<item>\n   written before the move
//   moved\t<rel>\t<item>/payload\n
//   absent\t<rel>\n
//   failed\t<rel>\t<errno>\n    the move could not be published; source kept
// Non-printable bytes are written \xNN so newline-bearing names cannot forge
// receipt lines.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#ifndef RENAME_NOREPLACE
#define RENAME_NOREPLACE (1U << 0)
#endif

static void die(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    fputs("retain-move: ", stderr);
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    _exit(2);
}

static void die_at(const char *what, const char *path) {
    die("%s: %s", what, path);
}

static char *canonical(const char *path) {
    char *out = realpath(path, NULL);
    if (!out) die_at("cannot resolve", path);
    return out;
}

// Open one directory component relative to `fd`, refusing symlinks. Missing
// or non-directory ancestors return -2 so the caller can apply the caller's
// absent-target policy without treating escape attempts as "absent".
static int open_dir_at(int fd, const char *name, size_t len, int *absent) {
    char buf[4096];
    if (len == 0 || len >= sizeof(buf)) die("path component too long");
    if (len == 2 && name[0] == '.' && name[1] == '.')
        die("refusing '..' in retirement path");
    memcpy(buf, name, len);
    buf[len] = 0;
    int out = openat(fd, buf, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (out < 0 && (errno == ENOENT || errno == ENOTDIR)) {
        *absent = 1;
        return -1;
    }
    if (out < 0) die_at("cannot open ancestor directory", buf);
    return out;
}

// Walk `rel` (no leading slash) below `rootfd`. On success returns the parent
// directory fd of the last component and stores that component in `last`.
// Returns -1 when an ancestor is absent; the caller decides whether "absent"
// is acceptable. A symlinked ancestor is always fatal — confinement can
// never be proven through one.
static int walk_ancestors(int rootfd, const char *rel, char *last, size_t last_len) {
    int fd = rootfd, absent = 0;
    const char *p = rel;
    while (*p == '/') p++;
    last[0] = 0;
    while (*p) {
        const char *slash = strchr(p, '/');
        size_t len = slash ? (size_t)(slash - p) : strlen(p);
        if (len == 1 && p[0] == '.') {
            p += len;
            while (*p == '/') p++;
            continue;
        }
        const char *rest = p + len;
        while (*rest == '/') rest++;
        if (*rest == 0) {
            // Last component: it may itself be "." (e.g. "a/.").
            if (len == 2 && p[0] == '.' && p[1] == '.')
                die("refusing '..' as retirement target");
            if (len == 0 || len >= last_len) die("path component too long");
            memcpy(last, p, len);
            last[len] = 0;
            return fd;
        }
        int next = open_dir_at(fd, p, len, &absent);
        if (next < 0) {
            if (fd != rootfd) close(fd);
            return -1;
        }
        if (fd != rootfd) close(fd);
        fd = next;
        p = rest;
    }
    return fd;
}

static void check_allowed(const char *rel) {
    static const char *trees[] = {"vendor/", "applets/", "scratch/"};
    size_t n = strlen(rel);
    for (size_t i = 0; i < sizeof(trees) / sizeof(trees[0]); i++)
        if (strncmp(rel, trees[i], strlen(trees[i])) == 0) return;
    if (n > 0) // targets must live under the run's generated trees only
        die("retirement path outside owned generated trees: %s", rel);
    die("empty retirement path");
}

static void esc(char *out, size_t out_size, const char *in) {
    size_t o = 0;
    for (const unsigned char *p = (const unsigned char *)in; *p; p++) {
        unsigned char c = *p;
        if (c >= 0x20 && c <= 0x7e && c != '\\') {
            if (o + 1 >= out_size) die("receipt path too long");
            out[o++] = (char)c;
        } else {
            if (o + 4 >= out_size) die("receipt path too long");
            int n = snprintf(out + o, out_size - o, "\\x%02x", c);
            if (n < 0) die("receipt encoding failed");
            o += (size_t)n;
        }
    }
    out[o] = 0;
}

static void index_append(int retired_fd, const char *line) {
    int fd = openat(retired_fd, "INDEX.tsv", O_WRONLY | O_APPEND | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) die_at("cannot open INDEX.tsv", "retired/INDEX.tsv");
    size_t len = strlen(line);
    const char *p = line;
    while (len) {
        ssize_t n = write(fd, p, len);
        if (n < 0) {
            int e = errno;
            close(fd);
            errno = e;
            die("INDEX append failed: %s", strerror(errno));
        }
        p += n;
        len -= (size_t)n;
    }
    if (fsync(fd) < 0) die("INDEX fsync failed: %s", strerror(errno));
    if (close(fd) < 0) die("INDEX close failed: %s", strerror(errno));
}

static void write_origin(int slot_fd, const char *rel) {
    int fd = openat(slot_fd, "ORIGIN.txt", O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600);
    if (fd < 0) die("ORIGIN.txt create failed: %s", strerror(errno));
    size_t len = strlen(rel);
    if (write(fd, rel, len) != (ssize_t)len || write(fd, "\n", 1) != 1)
        die("ORIGIN.txt write failed: %s", strerror(errno));
    if (fsync(fd) < 0) die("ORIGIN.txt fsync failed: %s", strerror(errno));
    close(fd);
}

static int do_rename(int parent_fd, const char *last, int slot_fd) {
    int rc = (int)syscall(SYS_renameat2, parent_fd, last, slot_fd, "payload", RENAME_NOREPLACE);
    if (rc < 0 && errno == ENOSYS)
        // No RENAME_NOREPLACE on this kernel: 'payload' was just created
        // empty inside an exclusive slot, so plain renameat cannot clobber.
        rc = renameat(parent_fd, last, slot_fd, "payload");
    return rc;
}

static void slot_name(char *out, size_t size) {
    unsigned char r[8];
    if (getrandom(r, sizeof(r), 0) != (ssize_t)sizeof(r))
        die("getrandom failed: %s", strerror(errno));
    snprintf(out, size, "item.%02x%02x%02x%02x%02x%02x%02x%02x",
             r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]);
}

int main(int argc, char **argv) {
    const char *run_arg = getenv("SHENKO_TOYBOX_RUN_DIR");
    if (!run_arg || !run_arg[0]) die("SHENKO_TOYBOX_RUN_DIR is not set");
    int force = 0, recursive = 0, i = 1;
    for (; i < argc; i++) {
        const char *a = argv[i];
        if (strcmp(a, "--") == 0) { i++; break; }
        if (a[0] != '-' || a[1] == 0) break;
        for (const char *f = a + 1; *f; f++) {
            if (*f == 'f') force = 1;
            else if (*f == 'r' || *f == 'R') recursive = 1;
            else die("unsupported flag -%c", *f);
        }
    }
    int targets = argc - i;
    if (targets <= 0) return 0; // an empty glob under -f removes nothing

    // The run root is opened once and kept: every later operation is
    // dirfd-relative, so ancestor swaps between validation and move cannot
    // redirect the archive outside the run.
    char *run = canonical(run_arg);
    int runfd = open(run, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (runfd < 0) die_at("cannot open run directory", run);
    int retired_fd = openat(runfd, "retired", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (retired_fd < 0) die_at("cannot open retirement archive", "retired");

    char *cwd = canonical(".");
    size_t run_len = strlen(run), cwd_len = strlen(cwd);

    for (; i < argc; i++) {
        const char *arg = argv[i];
        char rel_buf[4096];
        if (arg[0] == '/') {
            // Absolute paths must already sit under the canonical run.
            if (strncmp(arg, run, run_len) || arg[run_len] != '/')
                die("path outside run: %s", arg);
            snprintf(rel_buf, sizeof(rel_buf), "%s", arg + run_len + 1);
        } else {
            // Relative paths are anchored at the build's cwd, which must
            // itself live inside the run (the build cd's into vendor/).
            if (strncmp(cwd, run, run_len) ||
                (cwd[cwd_len] && cwd[cwd_len] != '/'))
                die("build cwd outside run: %s", cwd);
            const char *base = cwd[cwd_len] ? cwd + run_len + 1 : "";
            if (snprintf(rel_buf, sizeof(rel_buf), "%s%s%s",
                         base, *base ? "/" : "", arg) >= (int)sizeof(rel_buf))
                die("retirement path too long");
        }
        char rel[4096];
        {
            // Collapse a textual copy for policy checks; walk uses the same
            // bytes so nothing can disagree about which path was checked.
            snprintf(rel, sizeof(rel), "%s", rel_buf);
        }
        check_allowed(rel);

        char last[4096];
        int parent_fd = walk_ancestors(runfd, rel, last, sizeof(last));
        if (parent_fd < 0) {
            if (!force) die("retirement target is absent: %s", rel);
            char esc_rel[8192], line[9000];
            esc(esc_rel, sizeof(esc_rel), rel);
            snprintf(line, sizeof(line), "absent\t%s\n", esc_rel);
            index_append(retired_fd, line);
            continue;
        }
        if (last[0] == 0 || !strcmp(last, ".") || !strcmp(last, ".."))
            die("invalid retirement target: %s", rel);
        struct stat st;
        int exists = fstatat(parent_fd, last, &st, AT_SYMLINK_NOFOLLOW) == 0;
        if (!exists) {
            if (errno != ENOENT && errno != ENOTDIR)
                die_at("cannot stat retirement target", last);
            if (parent_fd != runfd) close(parent_fd);
            if (!force) die("retirement target is absent: %s", rel);
            char esc_rel[8192], line[9000];
            esc(esc_rel, sizeof(esc_rel), rel);
            snprintf(line, sizeof(line), "absent\t%s\n", esc_rel);
            index_append(retired_fd, line);
            continue;
        }
        if (S_ISDIR(st.st_mode) && !recursive)
            die("directory retirement requires -r: %s", rel);

        // Exclusive slot: mkdirat must create it, never reuse.
        char slot[64];
        int slot_fd = -1;
        for (int attempt = 0; attempt < 64; attempt++) {
            slot_name(slot, sizeof(slot));
            if (mkdirat(retired_fd, slot, 0700) == 0) {
                slot_fd = openat(retired_fd, slot, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
                break;
            }
            if (errno != EEXIST) die("retirement slot create failed: %s", strerror(errno));
        }
        if (slot_fd < 0) die("cannot allocate a retirement slot");

        write_origin(slot_fd, rel);

        // Receipt intent before the irreversible move: if this append fails,
        // nothing was moved and the source path still exists.
        char esc_rel[8192], esc_slot[512], line[9000];
        esc(esc_rel, sizeof(esc_rel), rel);
        esc(esc_slot, sizeof(esc_slot), slot);
        snprintf(line, sizeof(line), "pending\t%s\t%s\n", esc_rel, esc_slot);
        index_append(retired_fd, line);

        if (do_rename(parent_fd, last, slot_fd) < 0) {
            int e = errno;
            snprintf(line, sizeof(line), "failed\t%s\t%s\n", esc_rel, strerror(e));
            // Best effort: the failure receipt must not hide the rename error.
            int ifd = openat(retired_fd, "INDEX.tsv", O_WRONLY | O_APPEND | O_NOFOLLOW | O_CLOEXEC);
            if (ifd >= 0) { (void)!write(ifd, line, strlen(line)); close(ifd); }
            if (parent_fd != runfd) close(parent_fd);
            close(slot_fd);
            die("cannot archive %s: %s", rel, strerror(e));
        }

        snprintf(line, sizeof(line), "moved\t%s\t%s/payload\n", esc_rel, esc_slot);
        int ifd = openat(retired_fd, "INDEX.tsv", O_WRONLY | O_APPEND | O_NOFOLLOW | O_CLOEXEC);
        if (ifd < 0 || write(ifd, line, strlen(line)) != (ssize_t)strlen(line)
            || fsync(ifd) < 0) {
            // The bytes are safe inside the slot; restore the original name
            // so a failed receipt never strands the source under 'payload'.
            int rb = (int)syscall(SYS_renameat2, slot_fd, "payload", parent_fd, last, RENAME_NOREPLACE);
            if (rb < 0 && errno == ENOSYS) rb = renameat(slot_fd, "payload", parent_fd, last);
            if (ifd >= 0) close(ifd);
            if (rb < 0)
                die("INDEX append failed and restore failed; retained copy is at retired/%s/payload (original: %s)", slot, rel);
            die("INDEX append failed; source restored to %s", rel);
        }
        close(ifd);
        if (parent_fd != runfd) close(parent_fd);
        close(slot_fd);
    }
    return 0;
}
