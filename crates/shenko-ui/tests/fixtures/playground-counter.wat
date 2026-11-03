;; Real WASI preview1 command, not an interpreter surrogate.
;; Increment one binary byte in /home/counter.bin; output bounded JSON.
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read"
    (func $read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_seek"
    (func $seek (param i32 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 100) "counter.bin")
  (data (i32.const 160) "{\22ok\22:true}")
  (func (export "_start")
    ;; fd3 = /home. CREAT, FD_READ|FD_SEEK|FD_TELL|FD_WRITE = 102.
    (if (call $open (i32.const 3) (i32.const 0) (i32.const 100) (i32.const 11)
          (i32.const 1) (i64.const 102) (i64.const 0) (i32.const 0) (i32.const 200))
      (then (call $exit (i32.const 41))))
    ;; Read an existing byte (zero-initialized memory handles first creation).
    (i32.store (i32.const 208) (i32.const 240))
    (i32.store (i32.const 212) (i32.const 1))
    (if (call $read (i32.load (i32.const 200)) (i32.const 208) (i32.const 1) (i32.const 224))
      (then (call $exit (i32.const 42))))
    (i32.store8 (i32.const 240) (i32.add (i32.load8_u (i32.const 240)) (i32.const 1)))
    (if (call $seek (i32.load (i32.const 200)) (i64.const 0) (i32.const 0) (i32.const 248))
      (then (call $exit (i32.const 43))))
    (if (call $write (i32.load (i32.const 200)) (i32.const 208) (i32.const 1) (i32.const 224))
      (then (call $exit (i32.const 44))))
    ;; Structured stdout, exactly eleven bytes: {"ok":true}.
    (i32.store (i32.const 208) (i32.const 160))
    (i32.store (i32.const 212) (i32.const 11))
    (if (call $write (i32.const 1) (i32.const 208) (i32.const 1) (i32.const 224))
      (then (call $exit (i32.const 45))))))
