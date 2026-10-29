/* wasi-compat.h — forced-included (-include) shim for building toybox 0.8.15
 * under wasi-sdk (wasi-libc, WASI preview1).
 *
 * wasi-libc omits process-spawning, credential, terminal, and IPC interfaces
 * that have no preview1 capability. This header supplies ONLY what the
 * guarded call sites still reference. Nothing here pretends a syscall works:
 * every macro is an ENOSYS/ENOTTY failure or an honest ambient constant, and
 * the accompanying patches guard every real call site explicitly.
 */
#ifndef SHENKO_TOYBOX_WASI_COMPAT_H
#define SHENKO_TOYBOX_WASI_COMPAT_H

#ifdef __wasi__

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#include <stdint.h>
#include <stdlib.h>

/* Pull real headers where the libc ships them; only fall back when absent. */
#if defined(__has_include)
#  if __has_include(<pwd.h>)
#    include <pwd.h>
#  endif
#  if __has_include(<grp.h>)
#    include <grp.h>
#  endif
#  if __has_include(<termios.h>)
#    include <termios.h>
#  endif
#  if __has_include(<sys/ioctl.h>)
#    include <sys/ioctl.h>
#  endif
#endif

/* ---- credentials --------------------------------------------------------- */
#ifndef _PWD_H
struct passwd {
  char *pw_name; char *pw_passwd; uid_t pw_uid; gid_t pw_gid;
  char *pw_gecos; char *pw_dir; char *pw_shell;
};
#endif
#ifndef _GRP_H
struct group { char *gr_name; char *gr_passwd; gid_t gr_gid; char **gr_mem; };
#endif

/* uid/gid queries: WASI has one ambient identity; reporting 0 is honest —
 * callers treat it as root-equivalent only for ownership preservation,
 * which is then skipped by the per-site guards. */
static inline uid_t wasi_getuid(void)  { return 0; }
static inline uid_t wasi_geteuid(void) { return 0; }
static inline gid_t wasi_getgid(void)  { return 0; }
static inline gid_t wasi_getegid(void) { return 0; }
#define getuid()   wasi_getuid()
#define geteuid()  wasi_geteuid()
#define getgid()   wasi_getgid()
#define getegid()  wasi_getegid()
#define getppid()  ((pid_t)1)

/* ---- umask / timezone ------------------------------------------------------ */
/* WASI has no process umask; 022 is the ambient default wasi-libc applies. */
static inline mode_t wasi_umask(mode_t m) { (void)m; return 022; }
#define umask(m) wasi_umask(m)
#define tzset()  ((void)0)
#define sync()   ((void)0)

/* ---- process creation: honestly absent ------------------------------------- */
#define fork()   (errno = ENOSYS, (pid_t)-1)
#define vfork()  (errno = ENOSYS, (pid_t)-1)
#define execve(p, a, e)  (errno = ENOSYS, -1)
#define execv(p, a)      (errno = ENOSYS, -1)
#define waitpid(p, s, o) (errno = ENOSYS, (pid_t)-1)
#define kill(p, s)       (errno = ENOSYS, -1)
#define setsid()         (errno = ENOSYS, (pid_t)-1)
#define chroot(p)        (errno = ENOSYS, -1)
#define daemon(a, b)     (errno = ENOSYS, -1)
#define system(s)        (errno = ENOSYS, -1)
#define initgroups(u, g) (errno = ENOSYS, -1)
#define setuid(u)        (errno = ENOSYS, -1)
#define setgid(g)        (errno = ENOSYS, -1)

/* ---- ownership / device nodes ---------------------------------------------- */
#define chown(p, u, g)             (errno = ENOSYS, -1)
#define fchown(f, u, g)            (errno = ENOSYS, -1)
#define lchown(p, u, g)            (errno = ENOSYS, -1)
#define fchownat(d, p, u, g, f)    (errno = ENOSYS, -1)
#define mknod(p, m, d)             (errno = ENOSYS, -1)
#define mknodat(d, p, m, v)        (errno = ENOSYS, -1)
#define mkfifo(p, m)               (errno = ENOSYS, -1)
#define settimeofday(t, z)         (errno = ENOSYS, -1)
#define clock_settime(c, t)        (errno = ENOSYS, -1)

/* ---- terminal control -------------------------------------------------------- */
#ifndef _TERMIOS_H
struct termios { unsigned long _wasi_opaque[8]; };
#endif
#ifndef TIOCGWINSZ
#define TIOCGWINSZ 0x5413
struct winsize {
  unsigned short ws_row, ws_col, ws_xpixel, ws_ypixel;
};
#endif
/* Only referenced inside guarded bodies; macros exist so late definitions
 * still parse if a future site sneaks one in — it will fail ENOTTY, loudly. */
#define tcgetattr(f, t)    (errno = ENOTTY, -1)
#define tcsetattr(f, a, t) (errno = ENOTTY, -1)
#define tcflush(f, a)      (errno = ENOTTY, -1)
#define cfmakeraw(t)       ((void)0)
#define cfsetspeed(t, s)   (errno = ENOTTY, -1)
#ifndef TCIFLUSH
#define TCIFLUSH 0
#endif
#ifndef TCSAFLUSH
#define TCSAFLUSH 0
#endif
#ifndef TCSANOW
#define TCSANOW 0
#endif
#ifndef IUTF8
#define IUTF8 0
#endif
#ifndef ICRNL
#define ICRNL 0
#endif
#ifndef IXANY
#define IXANY 0
#endif
#ifndef OPOST
#define OPOST 0
#endif
#ifndef ONLCR
#define ONLCR 0
#endif
#ifndef CS8
#define CS8 0
#endif
#ifndef CREAD
#define CREAD 0
#endif
#ifndef ISIG
#define ISIG 0
#endif
#ifndef ICANON
#define ICANON 0
#endif
#ifndef ECHO
#define ECHO 0
#endif
#ifndef ECHOE
#define ECHOE 0
#endif
#ifndef ECHOK
#define ECHOK 0
#endif
#ifndef ECHOCTL
#define ECHOCTL 0
#endif
#ifndef ECHOKE
#define ECHOKE 0
#endif
#ifndef IEXTEN
#define IEXTEN 0
#endif
#ifndef SIGWINCH
#define SIGWINCH 28
#endif

/* ---- misc ------------------------------------------------------------------ */
#if defined(__has_include)
#  if !__has_include(<sys/statfs.h>)
/* wasi-libc omits statfs entirely; provide the shape so FLAG(f)-guarded code
 * still compiles. The syscall itself is an honest ENOSYS. */
struct statfs {
  long f_type, f_bsize;
  unsigned long long f_blocks, f_bfree, f_bavail, f_files, f_ffree;
  int f_fsid[2];
  long f_namelen, f_frsize, f_iosize, f_spare[6];
};
#  endif
#endif
#define statfs(p, b)   (errno = ENOSYS, -1)
#define fstatfs(f, b)  (errno = ENOSYS, -1)

#define getdtablesize()  ((int)sysconf(_SC_OPEN_MAX))
#ifndef _SC_OPEN_MAX
#define _SC_OPEN_MAX 31
#endif
#ifndef LOG_DAEMON
#define LOG_DAEMON (3<<3)
#endif
#ifndef LOG_AUTH
#define LOG_AUTH (4<<3)
#endif
#ifndef LOG_PID
#define LOG_PID 1
#endif

/* mkstemp is absent but toybox needs atomic temp creation (ln -f).
 * Re-implement with O_CREAT|O_EXCL so the guarantee is preserved. */
static inline int wasi_mkstemp(char *tmpl)
{
  char *x = tmpl + strlen(tmpl);
  while (x > tmpl && x[-1] == 'X') --x;
  for (int tries = 0; tries < 64; ++tries) {
    unsigned seed = (unsigned)tries * 2654435761u ^ (unsigned)(uintptr_t)tmpl;
    for (char *p = x; *p; ++p) {
      seed = seed * 31 + (unsigned)(p - x) + 7;
      *p = "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"[seed % 32];
    }
    int fd = open(tmpl, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd >= 0) return fd;
    if (errno != EEXIST) return -1;
  }
  errno = EEXIST;
  return -1;
}
#define mkstemp(t) wasi_mkstemp(t)

#endif /* __wasi__ */
#endif /* SHENKO_TOYBOX_WASI_COMPAT_H */
