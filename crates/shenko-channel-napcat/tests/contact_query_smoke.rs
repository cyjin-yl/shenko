//! Guarded real-service read-only probe recipe — SPEC §15.2, §16, §35.6.
//!
//! Loopback coverage (echo correlation, provenance, typed outcomes) lives in
//! the crate's unit tests; this file exists only as the opt-in live recipe.
//! `live_napcat_readonly_probe` runs solely when `SHENKO_NAPCAT_WS_URL` (and
//! optionally `SHENKO_NAPCAT_ACCOUNT`) point at a real NapCat instance the
//! operator deliberately selected. It issues *read-only* `get_*` actions
//! only — it never sends messages, never touches contacts, and asserts
//! nothing about live data except that the observed identity binding is
//! reported truthfully. Without the env vars it skips, keeping `cargo test`
//! deterministic and offline.
use shenko_channel_napcat::{
    endpoint_provenance, ContactQuery, NapCatClient, NapCatConfig, SendError,
};
use std::time::Duration;

fn client_for(url: String, account: u64) -> (NapCatClient, tokio::sync::mpsc::Receiver<shenko_channel_napcat::OneBotEvent>, tokio::sync::mpsc::Receiver<shenko_channel_napcat::OneBotAction>) {
    NapCatClient::new(NapCatConfig {
        ws_url: url,
        bot_qq: account,
        reconnect_interval_ms: 50,
        ..Default::default()
    })
}

/// Guarded real-service probe: read-only `get_*` actions against an actual
/// NapCat chosen by the operator. It never contacts people and never sends.
/// Run:
///   SHENKO_NAPCAT_WS_URL=ws://127.0.0.1:3001 SHENKO_NAPCAT_ACCOUNT=<bot qq> \
///   cargo test -p shenko-channel-napcat --test contact_query_smoke -- --nocapture
#[tokio::test]
async fn live_napcat_readonly_probe() {
    let Ok(url) = std::env::var("SHENKO_NAPCAT_WS_URL") else {
        eprintln!("skip: SHENKO_NAPCAT_WS_URL not set — no real NapCat selected");
        return;
    };
    // Endpoint provenance is credential-free by construction; panic/log text
    // must carry only the redacted form.
    let endpoint = endpoint_provenance(&url);
    let account: u64 = std::env::var("SHENKO_NAPCAT_ACCOUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let (client, _events, actions) = client_for(url.clone(), account);
    let cancel = tokio_util::sync::CancellationToken::new();
    let config = NapCatConfig {
        ws_url: url,
        bot_qq: account,
        ..Default::default()
    };
    let connector = tokio::spawn(NapCatClient::run_connection_loop(
        config,
        client.event_sender(),
        actions,
        client.pending_actions.clone(),
        client.spool.clone(),
        cancel.clone(),
    ));
    let probe = tokio::time::timeout(Duration::from_secs(20), async {
        let identity = client.query(ContactQuery::LoginInfo).await;
        let friends = client.query(ContactQuery::FriendList).await;
        let groups = client.query(ContactQuery::GroupList).await;
        (identity, friends, groups)
    })
    .await;
    cancel.cancel();
    let _ = connector.await;
    let (identity, friends, groups) = match probe {
        Ok(results) => results,
        Err(_) => panic!("probe deadline: no exchange completed against {endpoint}"),
    };
    for (label, result) in [
        ("get_login_info", &identity),
        ("get_friend_list", &friends),
        ("get_group_list", &groups),
    ] {
        match result {
            Ok(receipt) => {
                eprintln!(
                    "{label}: matched echo={} self_id={:?} retcode={} configured_account={}",
                    receipt.echo,
                    receipt.observed_self_id(),
                    receipt.receipt().retcode,
                    receipt.configured_account,
                );
                if account != 0 {
                    assert_eq!(
                        receipt.observed_self_id().unwrap_or(account),
                        account,
                        "{label}: response bound to a different account — not this bot's data"
                    );
                }
            }
            Err(SendError::Disconnected) => {
                eprintln!("{label}: connector unavailable — typed, not empty");
            }
            Err(SendError::Unknown(detail)) => {
                eprintln!("{label}: outcome unknown ({detail}) — typed, not empty");
            }
        }
    }
}
