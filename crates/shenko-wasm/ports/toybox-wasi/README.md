# toybox → WASI preview1 port (toybox 0.8.15)

A real port of the toybox userspace to WASI preview1, producing one
standalone `.wasm` per applet installable via `shenko tool install`.
This is the upstream source referenced by the `toybox` named target.

## Layout

- `wasi-compat.h` — `-include`d shim supplying the small set of types/macros
  wasi-libc lacks (see "what is stubbed" below; nothing fakes a working
  syscall — every stub is an `ENOSYS`/`ENOTTY` failure or ambient constant).
- `patches/` — `#ifdef __wasi__` guards in shared lib helpers and the toy
  call sites; upstream behavior on other platforms is untouched.
- `miniconfig.wasi` — the applets that genuinely work under preview1.
- `build.sh` — builds each applet via upstream `scripts/single.sh`
  (allnoconfig + one command → standalone binary).
- `ACQUISITION.md` — pinned upstream tarball + wasi-sdk-34 with checksums.
- `SMOKE.md` — post-build verification through `shenko tool exec`.

## Design constraints

- **No multiplexer.** `toybox`'s binary re-execs `/proc/self/exe` per applet;
  WASI has no process self-spawn. `single.sh` builds each NEWTOY command
  standalone, which is also what the per-tool unit record wants.
- **Honest failures only.** `fork/exec/socket/termios/mount/chown/mknod`
  have no preview1 capability; call sites for retained applets error out
  with a named reason, shared helpers that must still link get explicit
  `error_exit` stubs, and applets needing them are simply not configured.
- **umask/users/groups are ambient.** WASI has a single identity and no
  process umask; `toys.old_umask` is the wasi-libc ambient default (022),
  user/group lookups synthesize numeric entries, and `--preserve` ownership
  is skipped while mode/timestamp preservation still runs.

## What is stubbed (compat header)

`getuid/geteuid/getgid/getegid→0`, `umask→022`, `tzset/sync→noop`,
`fork/vfork/exec*/waitpid/kill/setsid/chroot/daemon/system→ENOSYS`,
`initgroups/set*id/chown*/mknod/mkfifo/settimeofday/clock_settime→ENOSYS`,
`tc*/cfmakeraw/cfsetspeed→ENOTTY` + minimal `struct termios`/`winsize` when
`termios.h`/`ioctl.h` are absent, `mkstemp→O_EXCL` loop (atomicity kept).
