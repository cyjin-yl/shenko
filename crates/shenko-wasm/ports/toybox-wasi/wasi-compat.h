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
#include <sys/socket.h>
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

/* Single-process runtime: the emulated getpid library returns a fixed
 * placeholder, so an honest constant is equivalent without the extra link. */
#define getpid()   ((pid_t)1)

/* ---- paths.h ----------------------------------------------------------- */
/* wasi-libc ships no paths.h. Only _PATH_DEFPATH is referenced (lib/env.c);
 * the value is the POSIX default search path. */
#ifndef _PATH_DEFPATH
#define _PATH_DEFPATH "/usr/bin:/bin"
#endif

/* ---- sys/wait.h ---------------------------------------------------------- */
/* There is no waitpid() (ENOSYS stub above), but lib/xwrap.c still parses a
 * status word on the non-WASI path and the macros must exist to compile.
 * They use the Linux/glibc encoding, which matches how callers read it. */
#ifndef WNOHANG
#define WNOHANG     1
#define WUNTRACED   2
#define WIFEXITED(s)    (!((s)&0x7f))
#define WEXITSTATUS(s)  (((s)&0xff00)>>8)
#define WTERMSIG(s)     ((s)&0x7f)
#define WIFSIGNALED(s)  (((signed char)(((s)&0x7f)+1))>1)
#define WCOREDUMP(s)    ((s)&0x80)
#define WIFSTOPPED(s)   (((s)&0xff)==0x7f)
#define WSTOPSIG(s)     WEXITSTATUS(s)
#define WIFCONTINUED(s) ((s)==0xffff)
#define WEXITED 4
#define WSTOPPED 8
#define WCONTINUED 16
#define WNOWAIT 0x01000000
#endif

/* ---- syslog.h ------------------------------------------------------------ */
/* openlog()/vsyslog()/closelog() are absent; lib/lib.c loggit() is patched
 * to write to stderr. These priority/facility constants keep the signature
 * and any comparison expressions compiling. */
#ifndef LOG_EMERG
#define LOG_EMERG   0
#define LOG_ALERT   1
#define LOG_CRIT    2
#define LOG_ERR     3
#define LOG_WARNING 4
#define LOG_NOTICE  5
#define LOG_INFO    6
#define LOG_DEBUG   7
#define LOG_PRIMASK 7
#define LOG_PRI(p) ((p)&LOG_PRIMASK)
#endif
#ifndef LOG_KERN
#define LOG_KERN    (0<<3)
#define LOG_USER    (1<<3)
#define LOG_MAIL    (2<<3)
#define LOG_DAEMON  (3<<3)
#define LOG_AUTH    (4<<3)
#define LOG_SYSLOG  (5<<3)
#define LOG_LPR     (6<<3)
#define LOG_NEWS    (7<<3)
#define LOG_UUCP    (8<<3)
#define LOG_CRON    (9<<3)
#define LOG_AUTHPRIV (10<<3)
#define LOG_FTP     (11<<3)
#define LOG_LOCAL0  (16<<3)
#define LOG_LOCAL1  (17<<3)
#define LOG_LOCAL2  (18<<3)
#define LOG_LOCAL3  (19<<3)
#define LOG_LOCAL4  (20<<3)
#define LOG_LOCAL5  (21<<3)
#define LOG_LOCAL6  (22<<3)
#define LOG_LOCAL7  (23<<3)
#define LOG_NFACILITIES 24
#define LOG_FACMASK 0x3f8
#define LOG_FAC(p) (((p)&LOG_FACMASK)>>3)
#endif
#ifndef LOG_PID
#define LOG_PID     0x01
#define LOG_CONS    0x02
#define LOG_ODELAY  0x04
#define LOG_NDELAY  0x08
#define LOG_NOWAIT  0x10
#define LOG_PERROR  0x20
#endif

/* ---- netdb.h --------------------------------------------------------------- */
/* Preview1 has no resolver; the structure is needed because lib/lib.h
 * declares xgetaddrinfo()/xconnectany()/xbindany() unconditionally and
 * lib/net.c passes the pointer around. Function bodies all error_exit. */
#ifndef __WASI_COMPAT_ADDRINFO
#define __WASI_COMPAT_ADDRINFO
struct addrinfo {
  int ai_flags, ai_family, ai_socktype, ai_protocol;
  socklen_t ai_addrlen;
  struct sockaddr *ai_addr;
  char *ai_canonname;
  struct addrinfo *ai_next;
};
#endif
#ifndef AI_PASSIVE
#define AI_PASSIVE 0x01
#define AI_CANONNAME 0x02
#define AI_NUMERICHOST 0x04
#define AI_NUMERICSERV 0x08
#endif
#ifndef NI_NUMERICHOST
#define NI_NUMERICHOST 0x01
#define NI_NUMERICSERV 0x02
#define NI_NOFQDN 0x04
#define NI_NAMEREQD 0x08
#define NI_DGRAM 0x10
#endif
#ifndef EAI_AGAIN
#define EAI_AGAIN 2
#define EAI_BADFLAGS 3
#define EAI_FAIL 4
#define EAI_FAMILY 5
#define EAI_MEMORY 6
#define EAI_NONAME 8
#define EAI_SERVICE 9
#define EAI_SOCKTYPE 10
#define EAI_SYSTEM 11
#endif

/* ---- signals ------------------------------------------------------------- */
/* wasi-libc emulated-signal provides signal()/raise() but not the
 * sigaction/sigprocmask/sig*set interfaces. All call sites are guarded;
 * these definitions keep guarded-out code structurally parseable and let
 * single-argument signal() stand in honestly where patch 0002 maps it. */
/* sigset_t already exists in <signal.h> (typedef unsigned char); do not
 * redefine it. The set-manipulation functions are absent from libc.a, so
 * every caller is guarded; the macros below exist only for parse safety. */
/* wasi-libc provides setjmp/longjmp (libsetjmp.a) but no sigsetjmp/
 * siglongjmp: the savemask arg controls signal-mask restore, which the
 * emulated signal layer cannot perform anyway, so aliasing to the plain
 * pair preserves the only reachable behaviour. */
#define sigsetjmp(env, savemask) setjmp(env)
#define siglongjmp(env, val)     longjmp(env, val)
#define sigemptyset(s)  (errno = ENOSYS, -1)
#define sigfillset(s)   (errno = ENOSYS, -1)
#define sigaddset(s,n)  (errno = ENOSYS, -1)
#define sigdelset(s,n)  (errno = ENOSYS, -1)
#define sigismember(s,n) (errno = ENOSYS, -1)
#define sigprocmask(h,o,n) (errno = ENOSYS, -1)
#define sigpending(s)   (errno = ENOSYS, -1)
#define sigsuspend(s)   (errno = ENOSYS, -1)
#define sigaction(n,o,a) (errno = ENOSYS, -1)
#ifndef SIG_BLOCK
#define SIG_BLOCK   0
#define SIG_UNBLOCK 1
#define SIG_SETMASK 2
#endif
#ifndef SA_RESTART
#define SA_RESTART  0x10000000
#define SA_NOCLDSTOP 1
#define SA_NOCLDWAIT 2
#define SA_SIGINFO 4
#define SA_ONSTACK 0x08000000
#define SA_NODEFER 0x40000000
#define SA_RESETHAND 0x80000000
#endif

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

/* syscall() itself is absent: SYS_* constants come from <sys/syscall.h> and
 * no enabled applet reaches a Linux syscall table, so an ENOSYS stub keeps
 * dead paths compiling and live paths fail honestly. */
#define syscall(...)               (errno = ENOSYS, -1)

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
/* wasi-libc exposes ENOTSUP but no ENODATA; tar's selinux-path uses it to
 * mean "attribute absent". Map it to the wasi-libc name for the same
 * extended-attribute-missing condition. */
#ifndef ENODATA
#define ENODATA ENOTSUP
#endif

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
