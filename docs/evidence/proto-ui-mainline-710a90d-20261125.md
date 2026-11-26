# Proto-UI mainline consumer upgrade — 2026-11-25

## Exact source and scope

Shenko advances the reviewed Proto-UI pin from
`6ca8f262a074ed19cb24b35d5cc199b5b0e23e08` to
`710a90df2ea57d9f93d7d09e43ebfb2b7b8e5057`, the upstream main head after
[Proto-UI #1859](https://github.com/Proto-UI/Proto-UI/pull/1859).
That is 149 upstream commits.

What changed upstream between the two pins:

- `packages/compiler`: only `README.md`, which re-binds props-update
  scheduling to the draft C-LIFECYCLE-0003 / C-LIFECYCLE-0002 contracts.
  The compiler source tree is unchanged.
- `spec/contracts`: no change. Only `spec/prototypes` (3 files) and
  `spec/tests` (2 files) changed, none for a family Shenko adopts.
- Adopted Shadcn families changed in 10 of 16: textarea, scroll-area,
  message-scroller, collapsible, drawer, input, resizable, tree,
  context-menu and dropdown (22 files, +628/−124). Adapter, core and
  module packages changed in 21 files, mostly positioning leases.
- `pnpm-lock.yaml` is byte-identical
  (`eae18430bd967c94def36809b6d3305fa156a96548b08215a18dfe8a43923c07`).
  The three public entries remain `0.3.0`.

## Observed checks

The archived `build/build.py` adapts the previous upgrade's driver to this
container's paths. Toolchain: Node 24.22.1, pnpm 10.32.1, from a fresh
shallow checkout of the exact upstream commit.

| Check | Actual result |
| --- | --- |
| Frozen upstream and consumer installation | Passed; lockfiles unchanged |
| Public dependency closure | 42 packages built |
| Authored Prototype regeneration and `pnpm run check` | Passed |
| Existing Node tests | 69 passed, 0 failed |
| Normal production build | Passed; source inputs unchanged (`sourceStable: true`) |
| Browser observers (Inspector, Tree, Files, IM) | **Not run** |

The first attempt stopped at the consumer install, because pnpm refused to
replace an existing `node_modules` without a TTY. The recorded run sets
`CI=true`; its stages are in `stages.jsonl`.

The browser observers require Chromium's sandbox as a non-root user. This
container runs as root, and creating a separate user here was not
authorized, so no App browser claim is made for this pin. CI's independent
`web_assets.py prepare/check` rebuild remains the gate for committed bytes.

Resource changes: `app.js` 1,391,532 → 1,400,581 bytes;
`proto-ui-tokens.generated.css` 178,963 → 203,205 bytes. The bundled
compiler changed by 928 bytes, from core and module code it bundles.

## Compile probe of adopted families

`probe/` contains a compile-only probe. It bundles the pinned compiler and
calls `compileFile` with `react-dom-source-v1` on every adopted family's
`*.proto.ts`, using each file's actual export. It does not mount, render or
compare output with the adapter, and it does not change which families
Shenko runs through the adapter.

- 29 of 48 parts compiled.
- Families with every part compiled (7): button, card, input,
  message-scroller, scroll-area, separator and toggle.
- Families with at least one rejected part (9): collapsible, context-menu,
  drawer, dropdown, resizable, tabs, textarea, toggle-group and tree.
- `family-probe.txt` records each rejected part's first diagnostic, for
  example:
  - tree: `PUI1005` (lexical binding read before initialization);
  - drawer/content: `PUI1007` (call-depth boundary);
  - context-menu: unsupported `asContextMenuInput` and
    `InputOriginAnchor` declarations.

## Evidence

[Raw evidence archive](proto-ui-mainline-710a90d-20261125.tar.gz).

Archive SHA-256: `9b728b4ccc416e331c3adbf67156fe868b55cb1f392129e0cca7a963783f9fb2`.

The manifest's `source_checkout`, `receipt_path` and `web_root` name this
container's paths. CI removes these run-specific fields, `built_at` and
the receipt digest before comparing.
