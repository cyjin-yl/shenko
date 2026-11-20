//! Real HTTP admission and installed native dispatch into two private tmux panes.
//! The console records every input byte. It is not an OMP/Claude identity proof.
use super::{installed, withdrawn};
use super::super::SeatSessionsPlugin;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use shenko_exec_ssh::seat::{SeatAdapter, SeatHandle, runtime::SeatRuntime, tmux::TmuxSeatAdapter};
use shenko_kernel::Supervisor;
use shenko_store::Store;
use shenko_subject::{NativeChildPlugin, NativeChildService};
use shenko_types::{ids::EntityId, operation::{OperationOutcome, OperationState}};
use std::{fs, net::SocketAddr, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[path = "../../../crates/shenko-store/tests/support/canonical_native.rs"]
mod canonical_native;
#[path = "../tests/support/process_runtime.rs"]
mod process_runtime;

const SELECTOR: &str = "runtime_sessions::tests::owner_native::authenticated_second_pane_replay_read_only_and_same_command_replacement";
const CONSOLE: &str = r#"import os, sys, tty
tty.setraw(0)
with open(sys.argv[1], 'ab', buffering=0) as received:
    os.write(1, b'SeatReady\r\n')
    while True:
        data = os.read(0, 4096)
        if not data:
            break
        received.write(data)
        os.fsync(received.fileno())
"#;

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

async fn tmux(args: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(Duration::from_secs(5),
        tokio::process::Command::new("tmux").args(args).kill_on_drop(true).output())
        .await.context("owned tmux client deadline")??;
    ensure!(output.status.success(), "owned tmux command failed: {}",
        String::from_utf8_lossy(&output.stderr));
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

async fn http(address: SocketAddr, route: &str, bearer: Option<&'_ str>, body: &Value) -> Result<(u16, Value)> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut socket = tokio::net::TcpStream::connect(address).await?;
        let bytes = serde_json::to_vec(body)?;
        // Credentials exist only in this private request, never in diagnostics.
        let auth = bearer.map(|value| format!("x-shenko-admin: {value}\r\n")).unwrap_or_default();
        socket.write_all(format!("POST {route} HTTP/1.1\r\nHost: {address}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).as_bytes()).await?;
        socket.write_all(&bytes).await?;
        let mut response = Vec::new();
        socket.read_to_end(&mut response).await?;
        let boundary = response.windows(4).position(|part| part == b"\r\n\r\n").context("HTTP header boundary")?;
        let headers = std::str::from_utf8(&response[..boundary])?;
        let status = headers.split_whitespace().nth(1).context("HTTP status")?.parse()?;
        let payload = serde_json::from_slice(&response[boundary + 4..])
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&response[boundary + 4..]).into()));
        Ok((status, payload))
    }).await.context("owner HTTP request deadline")?
}

async fn selected(adapter: &TmuxSeatAdapter, pane: &str) -> Result<SeatHandle> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(handle) = adapter.discover().await?.into_iter().find(|handle| handle.location_id == pane)
                && tmux(&["capture-pane", "-p", "-t", pane]).await?.contains("SeatReady") {
                return Ok(handle);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.context("owned pane did not become observable")?
}

fn records(store: &Store, operation: EntityId, seat: EntityId, lease: EntityId) -> Result<Value> {
    Ok(json!({"operation":store.external_operation(operation)?, "seat":store.seat(seat)?,
        "lease":store.seat_lease(lease)?, "current_lease":store.seat_current_lease(seat)?}))
}

fn no_input(root: &Path) -> Result<()> {
    for name in ["first.input", "second.input"] {
        ensure!(fs::read(root.join(name))?.is_empty(), "terminal input reached {name}");
    }
    Ok(())
}

async fn journey(root: &Path, first: &str, second: &str, command: &str) -> Result<()> {
    let adapter = TmuxSeatAdapter::local();
    let first_handle = selected(&adapter, first).await?;
    let second_handle = selected(&adapter, second).await?;
    let first_location = adapter.local_location(&first_handle).await?;
    let second_location = adapter.local_location(&second_handle).await?;
    ensure!(first_location != second_location && first != second, "fixture needs two exact panes");
    let first_screen = tmux(&["capture-pane", "-p", "-t", first]).await?;
    let second_screen = tmux(&["capture-pane", "-p", "-t", second]).await?;
    fs::write(root.join("selected-locators.json"), serde_json::to_vec_pretty(&json!({
        "first":first_location,"selected_second":second_location}))?)?;
    no_input(root)?;

    let store = Arc::new(Store::open(root.join("store.db"), root.join("cas"))?);
    let agent = canonical_native::initialize_subject_in_with_route(&store, b"{}", None)?;
    store.start_subject_dispatcher()?;
    let (head, _) = store.load_subject(agent)?;
    let owning_task = head.subject_root_task_id;
    let installation = EntityId::new();
    let delivery = shenko_ui::owner::prepare_owner_claim(&store, root, installation)?
        .context("fresh owner claim delivery")?;
    let claim: Value = serde_json::from_slice(&fs::read(delivery)?)?;
    let supervisor = Supervisor::new(head.software_generation.clone());
    supervisor.publish_services().await?;
    supervisor.spawn(NativeChildPlugin, (store.clone(), head.software_generation)).await?;
    let native = supervisor.context().try_service::<NativeChildService>()?;
    let owner = supervisor.spawn(SeatSessionsPlugin, store.clone()).await?;
    installed(&native, &supervisor)?;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = reservation.local_addr()?;
    drop(reservation);
    let server = shenko_ui::server::UiServer::new(address.port())
        .with_data_dir(root).with_store(store.clone()).with_native(native.clone())
        .with_owner(shenko_ui::owner::OwnerState { store:store.clone(), installation_id:installation, data_dir:root.into() });
    let server = tokio::spawn(server.run());
    let checked = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                ensure!(!server.is_finished(), "real UI server exited before readiness");
                if tokio::net::TcpStream::connect(address).await.is_ok() { return Ok::<_, anyhow::Error>(()); }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.context("real UI listener readiness deadline")??;
        let (status, granted) = http(address, "/api/owner/claim", None, &json!({"capability":claim["capability"]})).await?;
        ensure!(status == 200 && granted["outcome"] == "granted", "owner claim did not grant");
        let bearer = granted["response"]["admin_bearer"].as_str().context("fresh claim bearer")?;
        let request = json!({"request_id":"selected-second", "task_id":owning_task, "location":second_location});
        let route = "/api/owner/seats/local/attach";
        let (_, denied) = http(address, route, None, &request).await?;
        ensure!(denied["accepted"] == false && denied["code"] == "OWNER_REQUIRED", "unauthenticated attach accepted");
        let mut spoof = request.clone();
        spoof["principal_ref"] = json!("body authority is forbidden");
        ensure!(http(address, route, Some(bearer), &spoof).await?.0 == 422, "body principal was accepted");
        let (_, admitted) = http(address, route, Some(bearer), &request).await?;
        ensure!(admitted["accepted"] == true && admitted["replayed"] == false, "owner admission failed: {admitted}");
        let task: EntityId = serde_json::from_value(admitted["request_task_id"].clone())?;
        let operation: EntityId = serde_json::from_value(admitted["operation_id"].clone())?;
        let seat = EntityId::derived("local-seat-attach-seat/v1", operation.to_string().as_bytes());
        let lease = EntityId::derived("local-seat-attach-lease/v1", operation.to_string().as_bytes());
        ensure!(store.seat(seat)?.is_none() && store.find_external_operation(operation)?.is_none(),
            "HTTP performed binding or execution before installed dispatch");
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                native.run_once().await?;
                if store.find_external_operation(operation)?.is_some_and(|record| record.state == OperationState::Resolved)
                    && store.external_task(task)?.is_terminal() { return Ok::<_, anyhow::Error>(()); }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.context("installed attachment settlement deadline")??;
        ensure!(store.external_operation(operation)?.outcome == Some(OperationOutcome::Succeeded),
            "installed attachment did not settle successfully");
        let original = records(&store, operation, seat, lease)?;
        let durable = store.seat(seat)?.context("installed handler did not bind selected seat")?;
        let original_lease = store.seat_lease(lease)?.context("atomic original lease missing")?;
        ensure!(durable.location == second_location && durable.task_id == owning_task
            && durable.read_only_operation == Some(operation), "installed handler bound another location/task/mode");
        ensure!(original_lease.holder_task_id == owning_task && original_lease.seat_id == seat
            && original_lease.lease_epoch == 1, "installed handler changed original lease identity");
        ensure!(store.seat_task_seats(owning_task)?.len() == 1, "attachment created duplicate seats");
        ensure!(adapter.local_location(&first_handle).await? == first_location
            && adapter.local_location(&second_handle).await? == second_location, "attach changed a pane identity");
        no_input(root)?;
        ensure!(tmux(&["capture-pane", "-p", "-t", first]).await? == first_screen
            && tmux(&["capture-pane", "-p", "-t", second]).await? == second_screen, "attachment changed terminal output");
        supervisor.uninstall(&owner).await?;
        withdrawn(&native, &supervisor)?;
        let (_, replay) = http(address, route, Some(bearer), &request).await?;
        ensure!(replay["accepted"] == true && replay["replayed"] == true
            && replay["request_task_id"] == admitted["request_task_id"]
            && replay["operation_id"] == admitted["operation_id"], "withdrawn generation lost original request replay");
        let mut changed = request;
        changed["location"] = serde_json::to_value(&first_location)?;
        let (_, conflict) = http(address, route, Some(bearer), &changed).await?;
        ensure!(conflict["accepted"] == false, "request identity allowed a changed pane");
        ensure!(records(&store, operation, seat, lease)? == original, "replay or conflict mutated original records");
        fs::write(root.join("original-binding.json"), serde_json::to_vec_pretty(&original)?)?;
        Ok::<_, anyhow::Error>((operation, seat, lease, original))
    }.await;
    server.abort();
    let server_join = server.await;
    let stopped = tokio::time::timeout(Duration::from_secs(10), supervisor.stop()).await
        .context("installed generation stop deadline").and_then(|result| result);
    ensure!(server_join.as_ref().is_err_and(|error| error.is_cancelled()), "UI server did not join cancellation");
    stopped?;
    let (operation, seat, lease, original) = checked?;
    drop(native);
    drop(supervisor);
    drop(store);

    // This is the canonical reattachment consumer also used by the installed
    // generation. It reopens actual Store/CAS and never calls a fake handler.
    let store = Arc::new(Store::open(root.join("store.db"), root.join("cas"))?);
    let runtime = SeatRuntime::new(store.clone());
    let read_only = async {
        let report = runtime.reattach_from_store(owning_task).await?;
        ensure!(report.reattached == vec![second_handle.clone()] && report.incarnation_bumped.is_empty()
            && report.skipped.is_empty(), "reopen did not retain the original exact attachment");
        let (fleet, truncated) = runtime.fleet_view(Some(owning_task)).await?;
        ensure!(!truncated && fleet.len() == 1 && fleet[0].transport_live == Some(true), "reopened original not live");
        let caps = fleet[0].seat.capability.as_ref().context("live capability missing")?;
        ensure!(!caps.structured_answers && !caps.steer && !caps.interrupt && !caps.resume,
            "read-only attachment advertises input capability");
        let write = store.check_seat_write(lease, owning_task).err().context("read-only lease admitted a write")?;
        ensure!(format!("{write:#}").contains("seat_read_only"), "wrong durable write refusal");
        for error in [runtime.submit(second, "must-never-arrive").await.err().context("read-only submit succeeded")?,
            runtime.interrupt(second).await.err().context("read-only interrupt succeeded")?] {
            ensure!(format!("{error:#}").contains("seat_read_only"), "wrong runtime write refusal: {error:#}");
        }
        no_input(root)?;
        ensure!(records(&store, operation, seat, lease)? == original, "reopen or write refusal changed original records");
        Ok::<_, anyhow::Error>(())
    }.await;
    let closed = runtime.close().await;
    read_only?;
    closed?;
    // Same command, cwd, pane and server; only the real pane root is replaced.
    tmux(&["respawn-pane", "-k", "-t", second, "-c", root.to_str().context("UTF-8 fixture root")?, command]).await?;
    let replacement = selected(&adapter, second).await?;
    ensure!(replacement.cwd == second_handle.cwd && replacement.current_command == second_handle.current_command,
        "replacement fixture changed command or cwd");
    ensure!(replacement.process_identity != second_handle.process_identity, "real pane root identity did not change");
    let replacement_location = adapter.local_location(&replacement).await?;
    let reopened = SeatRuntime::new(store.clone());
    let rejected = async {
        let report = reopened.reattach_from_store(owning_task).await?;
        ensure!(report.reattached.is_empty() && report.incarnation_bumped.is_empty()
            && report.skipped.len() == 1 && report.skipped[0].0 == seat, "same-command replacement was adopted");
        let (fleet, _) = reopened.fleet_view(Some(owning_task)).await?;
        ensure!(fleet.len() == 1 && fleet[0].durable_only && fleet[0].transport_live != Some(true),
            "replacement was presented as the original live transport");
        ensure!(records(&store, operation, seat, lease)? == original, "replacement rewrote original authority");
        ensure!(adapter.local_location(&first_handle).await? == first_location, "second-pane replacement changed first pane");
        no_input(root)?;
        fs::write(root.join("replacement-refused.json"), serde_json::to_vec_pretty(&json!({
            "replacement":replacement_location,"original":original,"fleet":fleet,"skipped":report.skipped,
            "input_bytes":[fs::metadata(root.join("first.input"))?.len(),fs::metadata(root.join("second.input"))?.len()]}))?)?;
        Ok::<_, anyhow::Error>(())
    }.await;
    let closed = reopened.close().await;
    rejected?;
    closed?;
    Ok(())
}

#[test]
fn authenticated_second_pane_replay_read_only_and_same_command_replacement() -> Result<()> {
    if let Some(root) = std::env::var_os("SHENKO_SEAT_OWNER_NATIVE_CHILD") {
        let root = PathBuf::from(root);
        ensure!(std::env::var_os("TMUX").is_none() && std::env::var_os("TMUX_PANE").is_none(), "inherited user tmux selector");
        ensure!(std::env::var_os("TMUX_TMPDIR").as_deref() == Some(root.join("tmux").as_os_str()), "foreign tmux namespace");
        return tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
            fs::write(root.join("console.py"), CONSOLE)?;
            let first_command = format!("exec python3 -u {} {}", quote(&root.join("console.py")), quote(&root.join("first.input")));
            let second_command = format!("exec python3 -u {} {}", quote(&root.join("console.py")), quote(&root.join("second.input")));
            let first = tmux(&["-f", "/dev/null", "new-session", "-d", "-s", "owner-native", "-c", root.to_str().context("UTF-8 fixture root")?, "-P", "-F", "#{pane_id}", &first_command]).await?;
            let checked = tokio::time::timeout(Duration::from_secs(90), async {
                let second = tmux(&["new-window", "-d", "-t", "owner-native", "-c", root.to_str().context("UTF-8 fixture root")?, "-P", "-F", "#{pane_id}", &second_command]).await?;
                journey(&root, &first, &second, &second_command).await
            }).await.context("owner native journey deadline").and_then(|result| result);
            let cleanup = tmux(&["kill-server"]).await;
            match (checked, cleanup) {
                (Ok(()), Ok(_)) => Ok(()),
                (Err(error), Ok(_)) => Err(error),
                (Ok(()), Err(error)) => Err(error.context("owned tmux cleanup failed")),
                (Err(error), Err(cleanup)) => Err(error.context(format!("owned tmux cleanup also failed: {cleanup:#}"))),
            }
        });
    }
    // Set the namespace only on a fresh harness process; never mutate the
    // environment shared by concurrent Rust tests or target a user's server.
    // Explicit evidence roots retain originals; ordinary tests own a temporary
    // world and remove it only after a successful child journey and join.
    let (evidence, retain) = match std::env::var_os("SHENKO_NATIVE_EVIDENCE_DIR") {
        Some(root) => (PathBuf::from(root), true),
        None => (std::env::temp_dir(), false),
    };
    let root = evidence.join(format!("seat-{}", &EntityId::new().as_uuid().simple().to_string()[..8]));
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    fs::create_dir(root.join("tmux"))?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.args(["--exact", SELECTOR, "--nocapture", "--test-threads=1"])
        .env("SHENKO_SEAT_OWNER_NATIVE_CHILD", &root).env("TMUX_TMPDIR", root.join("tmux"))
        .env_remove("TMUX").env_remove("TMUX_PANE");
    let status = process_runtime::OwnedProcess::spawn(&mut command, &root, "owner-native")?
        .finish(Duration::from_secs(115))?;
    ensure!(status.success(), "native owner consumer failed; private originals and diagnostics at {}", root.display());
    if !retain {
        fs::remove_dir_all(&root)?;
    }
    Ok(())
}
