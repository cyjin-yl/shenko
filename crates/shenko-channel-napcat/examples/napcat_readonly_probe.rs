//! Operator-driven read-only probe against a real NapCat — SPEC §15.2, §16,
//! §35.6. Not a test and not a fixture: it issues *read-only* `get_*` actions
//! only — never sends messages, never contacts people — and prints the typed
//! outcome of each read so an operator can verify the identity binding is
//! reported truthfully.
//!
//! Run:
//!   SHENKO_NAPCAT_WS_URL=ws://127.0.0.1:3001 SHENKO_NAPCAT_ACCOUNT=<bot qq> \
//!   cargo run -p shenko-channel-napcat --example napcat_readonly_probe
//!
//! Exit codes: 0 = probe ran (each action's outcome is printed, including
//! typed failures); 1 = SHENKO_NAPCAT_WS_URL not set; 2 = probe deadline hit.
use shenko_channel_napcat::{
    endpoint_provenance, ContactQuery, NapCatClient, NapCatConfig, SendError,
};
use std::process::ExitCode;
use std::time::Duration;

#[tokio::main]
async fn main() -> ExitCode {
    let Ok(url) = std::env::var("SHENKO_NAPCAT_WS_URL") else {
        eprintln!("SHENKO_NAPCAT_WS_URL not set — no real NapCat selected");
        return ExitCode::from(1);
    };
    // Endpoint provenance is credential-free by construction; diagnostics must
    // carry only the redacted form.
    let endpoint = endpoint_provenance(&url);
    let account: u64 = std::env::var("SHENKO_NAPCAT_ACCOUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let config = NapCatConfig {
        ws_url: url,
        bot_qq: account,
        ..Default::default()
    };
    let (client, _events, actions) = NapCatClient::new(config.clone());
    let cancel = tokio_util::sync::CancellationToken::new();
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
        Err(_) => {
            eprintln!("probe deadline: no exchange completed against {endpoint}");
            return ExitCode::from(2);
        }
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
                if account != 0
                    && receipt.observed_self_id().unwrap_or(account) != account
                {
                    eprintln!(
                        "{label}: response bound to a different account — \
                         not this bot's data"
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
    ExitCode::SUCCESS
}
