# shenko-channel-napcat-link

Optional NapCatQQ transport backend for Shenko (SPEC §18.1, ecosystem
compatibility), implemented on the [`napcat-link`](https://crates.io/crates/napcat-link)
SDK. It is an alternative to the hand-rolled WebSocket client in
`shenko-channel-napcat` and is **not** wired into the binary by default.

## Probe result

Probed 2026-10-23 against `napcat-link 0.1.0-alpha.2` (crates.io, MIT, edition
2024).

- **Fetch: success.** `cargo add napcat-link@0.1.0-alpha.2` + `cargo fetch` in a
  scratch crate under `/tmp` resolved the crate and 146 locked packages.
- **Compile: success.** `cargo check` on the scratch crate compiled the SDK
  cleanly (Rust 1.93 nightly, `rsproxy` registry). No upstream build errors, no
  patch needed.
- **API: usable.** The public surface read from
  `~/.cargo/registry/src/<registry>/napcat-link-0.1.0-alpha.2/src/` is:

| Item | Signature (as found) |
| --- | --- |
| Client | `NapLink::builder(url: &str) -> NapLinkBuilder` |
| Builder | `.token(&str)`, `.connection_timeout_ms(u64)`, `.ping_interval_ms(u64)`, `.reconnect_enabled(bool)`, `.reconnect_max_attempts(u32)`, `.backoff_initial_ms(u64)`, `.backoff_max_ms(u64)`, `.backoff_multiplier(f64)`, `.log_level(LogLevel)`, `.api_timeout_ms(u64)`, `.api_retries(u32)`, `.build() -> Result<NapLink>` |
| Lifecycle | `NapLink::connect() -> Result<()>`, `.disconnect()`, `.state() -> ConnectionState`, `.is_connected() -> bool` |
| Events | `NapLink::subscribe() -> EventSubscription`; `EventSubscription::recv() -> Option<EventData>`, `.recv_filter(&str)`, `.recv_exact(&str)`; `EventData { name: String, data: serde_json::Value }` (raw OneBot 11 event JSON) |
| Send | `NapLink::api() -> &OneBotApi`; `OneBotApi.message: MessageApi`; `send_group_message(group_id: i64, message: Vec<MessageSegment>) -> Result<Value>`; `send_private_message(user_id: i64, message: Vec<MessageSegment>) -> Result<Value>` |
| Segments | `MessageSegment::text(impl Into<String>)`, `::at`, `::at_all`, `::image`, ... |
| Escape hatch | `NapLink::call_api(method: &str, params: Value) -> Result<Value>` |

Caveat: the SDK is an alpha. Its API may change without notice; the version is
pinned exactly in `Cargo.toml`. The wrapper compiles against this version, which
is the actual compatibility claim we can make.

## Interface

```rust
#[async_trait::async_trait]
pub trait ChannelTransport: Send + Sync {
    async fn run(&self, on_event: EventHandler) -> Result<()>;
    async fn send_group(&self, group_id: i64, message: &str) -> Result<()>;
    async fn send_private(&self, user_id: i64, message: &str) -> Result<()>;
}

pub type EventHandler = Arc<dyn Fn(serde_json::Value) + Send + Sync>;
```

`run` connects and pumps raw OneBot 11 event JSON (`post_type`, `message_type`,
...) into the handler. Sends take plain text; the transport wraps it in a
`MessageSegment::text` segment. Richer segment composition can be added later
without disturbing this interface (or via `NapCatLinkTransport::client()` /
`call_api`).

`open(TransportConfig) -> Result<Box<dyn ChannelTransport>>` picks the backend
compiled in: the real `NapCatLinkTransport` with the feature, `StubTransport`
without it.

## Feature flag

```toml
[dependencies]
shenko-channel-napcat-link = { path = "crates/shenko-channel-napcat-link" }
# or, to get the real SDK transport:
shenko-channel-napcat-link = { path = "...", features = ["napcat-link"] }
```

```bash
cargo check -p shenko-channel-napcat-link                     # stub, no SDK
cargo check -p shenko-channel-napcat-link --features napcat-link
```

`default = []`; `napcat-link = ["dep:napcat-link"]`.

## Why the default is off

- The SDK is **alpha** (0.1.0-alpha.2) — it may break on a patch/minor, so it
  should not enter the default dependency graph unpinned and unnoticed.
- It drags in ~15 direct / ~140 transitive crates, including `native-tls` and
  OpenSSL, which is a real cost for a crate most builds do not need.
- Shenko already ships a working, dependency-light OneBot transport in
  `shenko-channel-napcat`; this backend is an ecosystem-compatibility option,
  evaluated and available on demand.
- Default builds and CI stay hermetic and fast, and the stub makes the disabled
  state explicit at runtime instead of silently doing nothing.

## Verification

`cargo check -p shenko-channel-napcat-link` (default) and
`cargo check -p shenko-channel-napcat-link --features napcat-link` both pass on
the current workspace (see the crate's task record). The feature build is the
real proof that the SDK API mapped here exists with these signatures.
