# Acquiring inputs for the toybox→WASI port

Everything below is pinned; verify every checksum before building. The port
downloads nothing itself — all inputs are local artifacts you place here.

## 1. Upstream source

- **File:** `toybox-0.8.15.tar.gz`
- **URL:** https://landley.net/toybox/downloads/toybox-0.8.15.tar.gz
  (mirror: https://github.com/landley/toybox/releases/tag/0.8.15)
- **SHA-256:** `52452b68a6cf50f9a9728b0f844b03c4dd1f70463675b202e6cf0475bda3934f`

Keep this tarball on persistent storage. `build.sh` checks the pinned hash
before claiming a new run directory; it never builds in an existing tree:

```sh
sha256sum toybox-0.8.15.tar.gz
```

## 2. Private patched vendor tree

`build.sh` unpacks the pinned archive into `<run>/vendor/` and applies the
patches with `--fuzz=0`; the patch logs are retained in `<run>/logs/`.
Do not patch or build in an existing upstream or project source tree.
Upstream scripts normally delete generated files and their own output while
building. Patch `0008-build-retain-generated.patch` changes every cleanup in
single.sh/make.sh/genconfig.sh, portability.sh and the Makefile to call
`retain-move` — a small C helper `build.sh` compiles into the run with
`HOSTCC` before unpacking:

- every ancestor directory is opened `O_DIRECTORY|O_NOFOLLOW` relative to the
  run root fd, so a swapped symlink ancestor cannot redirect the archive;
- existing targets move via `renameat2(RENAME_NOREPLACE)` into a freshly
  `mkdirat`'d `retired/item.*` slot — same-filesystem rename only, `EXDEV`
  rejects instead of degrading into copy+unlink; there is no no-replace
  `renameat` fallback in either direction, so a rollback can never overwrite
  a concurrently recreated source;
- `INDEX.tsv` records `pending` before the move and `moved`/`absent`/`failed`
  after; if the final receipt cannot be written, the payload is renamed back
  so a failed receipt never strands the source;
- non-printable path bytes are `\xNN`-escaped in receipts, and the run dir is
  passed via `SHENKO_TOYBOX_RUN_DIR` (environment bytes, immune to the
  trailing-newline loss of command substitution);
- upstream's unquoted `dirname $OUTNAME`, generated `-o $OUTNAME`, `mktemp`
  alias handling and `brun` paths are quoted for run dirs containing spaces;
  composite flag strings (`$BUILD`, `$CFLAGS`) keep intentional word splitting
  — full space-in-path support of the whole toolchain is *not* claimed without
  native evidence;
- the `LIBRARIES` probe loop is serialized — upstream ran each compiler probe
  in the background, bypassing `CPUS=1`; the generated `.singlemake` clean
  target escapes `$$SHENKO_TOYBOX_RETAIN` so make passes the shell variable.

### WASI preview1 header/type gaps closed by later patches

`0009-toys-h-wasi-includes.patch` guards every unconditional `toys.h`
include that does not exist in the wasi-sdk-34 wasm32-wasip1 sysroot:
`grp.h`, `pwd.h`, `paths.h`, `syslog.h`, `termios.h`, `sys/wait.h`,
`netdb.h`, `net/if.h`, `sys/mman.h`, `sys/resource.h`, `sys/times.h`.
`sys/mman.h`/`sys/resource.h`/`sys/times.h` physically exist but `#error`
unless `-D_WASI_EMULATED_MMAN`/`-D_WASI_EMULATED_PROCESS_CLOCKS` are passed;
no enabled applet calls mmap/getrlimit/times, so the headers stay out rather
than linking an emulation nobody uses.

`0010-lib-wasi-guards.patch` guards the remaining lib call sites the missing
headers imply: `xgetpwuid/xgetgrgid/xgetpwnam/xgetgrnam` (no passwd/group
database), `xwaitpid`/`xmmap`/`xvfork`/`xpopen`/`xrun`/`xexec`/`xchroot`/
`xsetuser` (no processes or privilege boundaries), the `cf*`/`tc*` baud-rate
table in `lib/tty.c` (no termios), `sigprocmask`/`sigfillset` in
`exit_signal` (no POSIX signal mask), `xnotify_*` (no inotify/kqueue),
`xattr_*` (no extended attributes — honest ENOSYS returns, not fake
success), `xbind`/`xconnect`/`pollinate`/`ntop`/`xsendto`/`xrecvwait` (no
sockets), `dev_makedev`/`fs_type_name`, and `_PATH_DEFPATH`/addrinfo/wait
W*/syslog LOG_* constants now live in `wasi-compat.h`.

`wasi-compat.h` additionally supplies: `struct passwd`/`struct group` when
the headers are absent, an opaque `struct termios`, `struct statfs` (shape
only — `stat()`/`fstatfs` are ENOSYS), `struct addrinfo`, the `W*` status
decoders using the Linux encoding `xwaitpid()` parses, `sigsetjmp/
siglongjmp` mapped onto the real `setjmp`/`longjmp` pair in `libsetjmp.a`,
`syscall()` as ENOSYS, `getpid()`/`getppid()` as the honest single-process
constant, `_PATH_DEFPATH`, the `LOG_*` syslog constants, `AI_*`/`NI_*`/`EAI_*`
and `SIG_BLOCK`/`SA_*` for guarded code that still must parse.

`build.sh` adds `-mllvm -wasm-enable-sjlj` (required by `<setjmp.h>` on
wasip1), `-lsetjmp` (the only setjmp implementation the sysroot ships), and
`LDOPTIMIZE=-Wl,--gc-sections` because wasm-ld rejects upstream's
`-Wl,--as-needed` fallback.

### Applet exclusions (impossible, not stubbed)

- `uptime` — neither `sysinfo()` nor a utmp database exists; there is no
  honest boot time, load average, or user count to report. Removed from
  `miniconfig.wasi`, not faked.
- Any applet needing `fork`/`exec`/`waitpid`, sockets, a terminal line
  discipline, `chown`/privilege changes, `/proc`, device nodes, or xattrs
  that must succeed. The code paths remain but `error_exit`/`ENOSYS`, so
  the binary can never claim the operation happened.

`GENDIR`, `UNSTRIPPED`, config paths and `TMPDIR` are confined to the run;
retirement rejects paths or ancestors outside its vendor/applets/scratch
trees. The product miniconfig is copied, never moved or offered as the
generated-config output. Builds enforce `CPUS=1` and `MAKEFLAGS=-j1`.
This retention patch/helper has not been executed or build-verified by the
worker; guarded parent verification must check the real archive receipts.

## 3. WASI SDK

- **File:** `wasi-sdk-34.0-x86_64-linux.tar.gz`
- **URL:** https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34
- **Release asset:** id `528366474`, size `192383077` bytes
- **SHA-256 (release-asset metadata digest):**
  `b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4`

The release publishes no `SHA256SUMS` file; verify the downloaded tarball
byte count and `sha256sum` against the asset metadata above (bound to tag
`wasi-sdk-34`), not a signed manifest.

Pass the extracted SDK directory, pinned source archive and a fresh
persistent run path (its parent must exist; the run itself must not):

```sh
tar xf wasi-sdk-34.0-x86_64-linux.tar.gz
./build.sh /path/to/wasi-sdk-34.0-x86_64-linux /path/to/toybox-0.8.15.tar.gz \
  /tank/shared/projects/shenko/build/c06-toybox-wasi-run-$(date +%Y%m%d-%H%M%S)
```

The SDK must contain `bin/clang` and `share/wasi-sysroot`; `build.sh`
compiles a retained probe (`<run>/probe.wasm`) first and exits clearly if
the SDK is wrong. No cleanup trap deletes the probe or any earlier run.

## 4. Host build tools

Toybox generates its config/flags headers with a *host* compile of
`kconfig/` and `scripts/` helpers. Requires: `cc` or `gcc`, `make`, `bash`,
`sed`, `egrep`, `xargs`, `tr`. No network access is used.

## 5. Output

`build.sh` writes `<run>/applets/<applet>` — one standalone WASI preview1
module per enabled applet — plus `probe.c`/`probe.wasm` and
`BUILD-RECEIPT.txt` (sdk path, source archive/hash, per-applet sha256). Each is
installable with:

```sh
shenko tool install --name toybox-<applet> --version 0.8.15 \
  --abi wasi_preview1 --file <run>/applets/<applet>
```

Run `SMOKE.md`'s checks before publishing artifacts.
