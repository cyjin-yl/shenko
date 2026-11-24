//! Live Discord acceptance — the opt-in real-platform round trip.
//!
//! The repository rule is that an unexecuted source claim is not evidence
//! (`docs/specs/channels.md`, "channel acceptance evidence level"). This test
//! is the executable half for the Discord bot transport:
//!
//! * without `SHENKO_DISCORD_LIVE_BOT_TOKEN` and
//!   `SHENKO_DISCORD_LIVE_CHANNEL_ID` it **skips with an explicit reason** and
//!   contacts nothing;
//! * with them it connects the Bot Gateway, receives a real message that the
//!   harness created, replies, reads the reply back, edits it, proves typing,
//!   retracts it and confirms the retraction, then records the capability
//!   table's live basis in a receipt.
//!
//! The harness never reads a credential from a file, never prints it and never
//! writes it to the receipt. The receipt's own JSON is checked against the
//! environment's token before this test reports a result.
//!
//! Run it explicitly:
//!
//! ```text
//! SHENKO_DISCORD_LIVE_BOT_TOKEN=... \
//! SHENKO_DISCORD_LIVE_CHANNEL_ID=123456789012345678 \
//! SHENKO_DISCORD_LIVE_RECEIPT=build/discord-live/receipt.json \
//!   cargo test -p shenko-channel-adapters --test discord_live_acceptance -- --nocapture
//! ```

use shenko_channel_adapters::discord_live::{
    CHANNEL_ENV, RECEIPT_SCHEMA_VERSION, TOKEN_ENV, DiscordLiveStatus, run_from_env,
    run_with_lookup,
};

/// The skip path must be exercised even on a machine that has credentials:
/// the lookup seam answers nothing, so no environment variable can make this
/// test reach the network.
#[test]
fn live_discord_acceptance_skips_with_an_explicit_reason_when_the_environment_is_absent() {
    let receipt = run_with_lookup(|_| None);
    assert_eq!(
        receipt.status,
        DiscordLiveStatus::Skipped,
        "an absent environment is a skip, not a failure"
    );
    assert_eq!(receipt.schema_version, RECEIPT_SCHEMA_VERSION);
    let skip = receipt
        .skip
        .as_ref()
        .expect("the skip reason is recorded in the receipt");
    assert!(
        skip.missing_environment.contains(&TOKEN_ENV.to_string()),
        "the absent token variable is named"
    );
    assert!(
        skip.missing_environment.contains(&CHANNEL_ENV.to_string()),
        "the absent channel variable is named"
    );
    assert!(
        !skip.network_attempted,
        "a skip from an absent environment contacts nothing"
    );
    assert!(
        skip.reason.contains(TOKEN_ENV) && skip.reason.contains(CHANNEL_ENV),
        "the reason is explicit: {}",
        skip.reason
    );
    assert!(
        !skip.prompt.is_empty(),
        "the receipt tells the operator the exact variables to provide"
    );
    assert!(
        receipt.failures.is_empty(),
        "a skip is not a failure: {:?}",
        receipt.failures
    );
    receipt
        .validate()
        .expect("a skipped receipt satisfies its own contract");
    eprintln!("SKIP discord_live_acceptance: {}", skip.reason);
}

/// The live round trip. Without credentials this prints the same explicit
/// skip; with them it fails loudly on any stage the platform did not accept.
#[test]
fn live_discord_acceptance() {
    let receipt = run_from_env();

    // A receipt is never allowed to carry the credential, whatever happened.
    if let Ok(token) = std::env::var(TOKEN_ENV) {
        let token = token.trim();
        if !token.is_empty() {
            let text = serde_json::to_string(&receipt.to_json())
                .expect("the receipt JSON serializes for the leak check");
            assert!(
                !text.contains(token),
                "the receipt leaked the configured bot token"
            );
        }
    }
    receipt
        .validate()
        .unwrap_or_else(|error| panic!("live Discord receipt is inconsistent: {error}"));

    match receipt.status {
        DiscordLiveStatus::Skipped => {
            let skip = receipt.skip.as_ref().expect("the skip reason is recorded");
            eprintln!("SKIP discord_live_acceptance: {}", skip.reason);
        }
        DiscordLiveStatus::Passed => {
            eprintln!(
                "PASS discord_live_acceptance: account {} channel {} received {} reply {} elapsed {}ms",
                receipt
                    .account
                    .as_ref()
                    .map(|account| account.user_id.as_str())
                    .unwrap_or("<unknown>"),
                receipt
                    .channel
                    .as_ref()
                    .map(|channel| channel.channel_id.as_str())
                    .unwrap_or("<unknown>"),
                receipt
                    .received
                    .as_ref()
                    .map(|received| received.message_id.as_str())
                    .unwrap_or("<none>"),
                receipt
                    .actions
                    .iter()
                    .find(|action| action.action == "reply")
                    .and_then(|action| action.provider_message_id.as_deref())
                    .unwrap_or("<none>"),
                receipt.elapsed_ms
            );
            if let Ok(path) = std::env::var(shenko_channel_adapters::discord_live::RECEIPT_ENV)
                && !path.trim().is_empty()
            {
                eprintln!("receipt: {path}");
            }
        }
        DiscordLiveStatus::Failed => {
            panic!(
                "live Discord acceptance failed; failures: {}",
                serde_json::to_string_pretty(&receipt.failures)
                    .unwrap_or_else(|_| "<unserializable failures>".to_string())
            );
        }
    }
}
