# UCP annotation resolver provenance

This directory contains the pure annotation resolver from [Universal Commerce Protocol's ucp-schema](https://github.com/Universal-Commerce-Protocol/ucp-schema), release **1.4.1**, upstream commit `6dd2a613d38828c97dca723bdb9ccfc79b2a7e80`. The original crate is Apache-2.0; its complete license is preserved in [LICENSE](LICENSE). No upstream NOTICE file was distributed with this crate.

Source: <https://crates.io/crates/ucp-schema/1.4.1>.

| Original source | SHA-256 before modification |
| --- | --- |
| `src/resolver.rs` | `76472afecb0396564a11b6a5a94221cf08b2dd8b2aec94194760aa1cec2a1c94` |
| `src/types.rs` | `f0e878b18a29d130b428fdf7003d98a9d1dd5ce2228d09a53b2f097bd0a0c8a4` |
| `src/error.rs` | `09b989fbf421a4d057f933ff984a8a166ce6987101f383e908bddb77db4d0ba5` |
| `src/loader.rs` | `25014ecf48db691e03018e02f104479ee918635a215ad3a5f07d660eb857fbe0` |
| `LICENSE` | `c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4` |

## Local changes

- Keep the production resolver and the types it actually uses, including visibility transitions, allOf annotation propagation, monotonicity checks, and version requirements.
- Remove upstream tests, CLI, file/network loaders, composition I/O, and unused convenience APIs. Fetching and bounded reference closure belong to Shenko's host transport.
- Adjust private module paths; implement the five pure resolver errors' original Display messages directly instead of taking a thiserror dependency.
- Use the workspace's existing serde_json map representation. Do not enable preserve_order: JSON object ordering has no schema meaning, and changing that global feature would change unrelated Shenko CAS serialization.
- `../rest_schema.rs` adapts the loader's inlining/fragment behavior to an explicitly supplied in-memory document map, with depth/node bounds. Recursive edges remain absolute references into that same supplied, annotation-resolved document set. Unbound anchors and dynamic references fail closed. It does not copy the filesystem or HTTP loader.
- Apply rustfmt and equivalent clippy cleanups without changing annotation semantics.

This code does no network or filesystem I/O. Its caller bounds documents before resolution and supplies every external schema explicitly to the JSON Schema validator. Unsupported or missing reference bindings fail closed; they do not trigger a remote loader.
