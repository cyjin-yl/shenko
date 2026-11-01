# Desktop runtime contract (computer.use, OS/VM providers, original LayerFS)

Scope: SPEC §10.2.1, §22, §24, §34; work package C15 (ArchDesktop).
Sources: `crates/shenko-exec-host/src/desktop.rs` (neutral contract),
`computer_use.rs` (product + X11 provider + native task kinds),
`desktop_windows.rs`, `desktop_macos.rs` (real FFI providers),
`layerfs.rs` + `scripts/layerfs-companion.mjs` (original LayerFS VM adapter),
registration in `bin/shenko/src/main.rs`.

## Kinds and callers

Six Cordis-registered native task kinds share one `ComputerUseBackend`
(store + provider registry + session map + human-input fanout + LayerFS
adapter). Registration is real in `bin/shenko/src/main.rs`:

| kind | handler | verb surface |
| --- | --- | --- |
| `computer.use.session` | `ComputerUseSessionHandler` | `{"backend":"x11"\|"windows"\|"macos"\|"auto","display":…,"screen":…,"op":"open"\|"close"}` |
| `computer.use.observe` | `ComputerUseObserveHandler` | `{"session_id","region":[x,y,w,h]\|null}` |
| `computer.use.lease` | `ComputerUseLeaseHandler` | `{"session_id","op":"acquire"\|"release"\|"takeover","lease_token"?}` |
| `computer.use.input` | `ComputerUseInputHandler` | `{"session_id","lease_token","observation_id":"blake3:…","actions":[…],"require_pixel_fresh"?}` |
| `computer.use.capabilities` | `ComputerUseCapabilitiesHandler` | `{"backend":…\|null}` |
| `computer.use.vm` | `ComputerUseVmHandler` | `{"op":"start"\|"stop"\|"state"\|"commit_receipt","name":"vm-name"}` |

Every operation is ledger-armed by `drive()`: frozen input sealed before any
external effect, resource binding + acceptance + no-retry policy sealed,
receipts only from what the transport actually answered.

## Neutral provider contract (`desktop.rs`)

`DesktopProvider` (`open`/`observe`/`lease_op`/`inject`/`close`/`reconcile`/
`capabilities`) + `DesktopSession` (`session_id`/`provider_id`/`domain_id`/
`display`/`is_closed`). Shared types: `DesktopDisplay` (DPI, scale factor,
depth, capability facts), `DesktopFrame` (raw provider-native pixels + RGB8
preview + `frame_id` + per-session monotonic `revision` + `lease_epoch` +
layout fingerprint), `DesktopObservation` (sealed record; its blake3 digest
is the `observation_id`), `DesktopAction`/`DesktopPress` (serde-tagged, same
JSON the X11 kinds always accepted), `DesktopLeaseState` + `apply_lease_op`
(the single acquire/release/takeover algorithm every provider uses, with the
epoch bump and machine-held-input release atomic under the domain lock —
handover fence), typed `DesktopError::{Unavailable,Rejected,Lost}`.

Domain identity spelling is owned here and shared with the browser/mobile
peers by contract: `x11/{socket_endpoint_key}/root{root:08x}` where
`socket_endpoint_key` is `tcp:{peer}` or `unix:path:{hex}`/`unix:abstract:{hex}`
of the live X11 connection.

## Observation identity, revision, DPI (SPEC §24.3)

- `revision` is per-session monotonic, starts at 1; every published
  observation increments it (ordering without pixel compares). `frame_id` is
  the provider's capture counter.
- Observation record carries `session_id`, `provider`, `domain_id`,
  `display_id`, `screen_num`, `frame_id`, `revision`, `captured_at_ms`,
  `lease_epoch_at_capture`, `region`, `viewport`, `dpi_x/y`, `scale_factor`,
  `coordinate_transform`, `layout_fingerprint`, `pixel_hash` (blake3 of the
  sealed raw pixels), `pixel_format`, `raw_pixels_ref`, `png_preview_ref`.
- The product seals raw pixels (ZPixmap/BGRA/RGBA provider bytes), the PNG
  preview, and the record; the record digest is the durable `observation_id`.
- Input must carry the `observation_id`; the product re-reads the sealed
  record (CAS), checks identity + the 300 s freshness window, and the
  provider re-verifies the live layout fingerprint (and exact pixels under
  `require_pixel_fresh`) plus the lease epoch before any event. Stale
  coordinates are never retargeted.

## Write lease, takeover, cancel, reconcile (SPEC §24.3/§10.6)

- One lease per interactive display domain; multiple read-only observers
  are free. Lease binds the controller instance, not a finite task id.
- `takeover` always revokes the old epoch (late machine input rejected) and
  releases machine-held keys/buttons before the new writer can emit.
- The X11 watcher (one LIVE thread per domain, only when XI >= 2.1 was
  negotiated AND raw selection ACKed on its own connection) revokes on any
  non-XTEST XI2 raw input, releases/restores, then dispatches the domain id
  through `HumanInputFanout` so the browser provider drops its agent lease
  on the same domain (integration: Main wires the 3-line adapter impl +
  `register_human_input_observer` in main.rs; signature `on_human_input(&self,
  domain_id: &str)`).
- Input batches call the product's durable cancel probe (`store.get_task`,
  committed `abort_request`) before every atomic send unit; cancellation
  releases held inputs and reports partial facts (`result:"cancelled"`),
  never replay.
- Reconcile never replays: armed ops without conclusive evidence stay
  Unknown with in-memory progress facts (`sent_events_before_recovery`,
  `session_alive_in_memory`, `process_stop_proven:false`).

## X11 provider (source-wired)

x11rb 0.13.2, real `RustConnection`. Open probes XTEST + XInput2 versions,
screen geometry, DPI (mm → dpi), display domain id; session JSON preserves
the historical top-level names (`xtest_version`, `xinput2_version`,
`xtest_source_device_ids`, `domain_id`, …). Observation is real `GetImage`
(ZPixmap) + window-tree layout fingerprint; input is real XTEST
`FakeInput` with per-event lease re-checks, server grab around keymap
resolution, keycode remapping for unmapped keysyms with verified restore,
held-input tracking that never adopts foreign holds, and `send_log` for
recovery. Non-X11 backends and unreachable displays fail typed-unavailable
naming the exact missing capability; no human desktop is ever guessed.

## Windows / macOS providers (real FFI, no external OS crates)

Both are cfg-gated real implementations against the system APIs:

- Windows (`desktop_windows.rs`): `user32`/`gdi32` hand-rolled FFI.
  Capture: `GetDC`/`BitBlt`/`GetDIBSection` top-down 32bpp BGRA; input:
  `SendInput` with VK/scancode/UNICODE events; lease per input desktop
  (`OpenInputDesktop` name). Human-input detection honestly reports
  `available:false` (needs an LLMHF hook message loop owned by this
  process); takeover stays lease-bound.
- macOS (`desktop_macos.rs`): CoreGraphics/ApplicationServices FFI.
  Capture: `CGDisplayCreateImage` + data provider bytes (BGRA8), with crop
  to region; input: `CGEventPost(kCGHIDEventTap)` mouse/keyboard events,
  unicode via `CGEventKeyboardSetUnicodeString`; lease per CG display.
  Screen Recording grant absence is a typed-unavailable naming it;
  Accessibility trust is reported per session.

## Original LayerFS on a qualified Linux VM (SPEC §10.2.1, §34)

`layerfs.rs` + `scripts/layerfs-companion.mjs` never re-implement the
LayerFS Store. The pinned ORIGINAL SDK `v1.0.0@2aaf2552…` is bound at
runtime by export-surface probe (`Store` required; `Branch`/`Workspace`
reported); mismatch stops the adapter typed. A per-VM Node companion drives
a qualified Linux VM provider (BoxLite optional first candidate) and runs
the upstream SDK in the guest over the provider's exec channel. The
`computer.use.vm` kind is the registered caller: start/stop/state mutate or
read the named VM only; `commit_receipt` returns the upstream
`layerfs_committed` receipt and never `product_durable`. Guest capability
facts (Docker/FUSE/mounts for the full workspace path) are probed, not
assumed. Missing SDK path or provider is typed-unavailable open evidence,
not completion. Env: `SHENKO_LAYERFS_SDK_PATH`, `SHENKO_LAYERFS_VM_PROVIDER`,
`SHENKO_LAYERFS_NODE`, `SHENKO_LAYERFS_COMPANION_SCRIPT`.

## Native proof recipe (parent-serialized)

On a host with a real X server (`Xvfb :99` qualifies as a real X11 server):

1. `DISPLAY=:99 cargo run -p shenko` — six kinds registered.
2. `computer.use.capabilities` `{}` → registry lists `x11`.
3. `computer.use.session` `{"backend":"x11","op":"open"}` → session JSON with
   probed XTEST/XInput2 + DPI.
4. `computer.use.observe` → `observation_id` + sealed raw/preview/record.
5. `computer.use.lease` acquire → token; `computer.use.input` with a
   pointer_move/key/text action bound to the observation → receipts with
   per-action transport; `pointer_post` in the result.
6. Type into the X server with a second client while the lease is held →
   lease revoked, held inputs released, human-input fanout dispatched.
7. `computer.use.vm` with unset env → typed-unavailable naming the missing
   SDK path/provider (honest open evidence); with a pinned SDK checkout and
   a qualified VM provider, start/stop report the provider's real state.

Not yet executed here (source phase; parent serializes integrated native
verification): all of the above, plus Windows/macOS builds
(`cargo check --target x86_64-pc-windows-msvc`, `cargo check
--target aarch64-apple-darwin`) for the cfg-gated providers.
