//! Local provisioning and read-only diagnostics. Business actions remain native
//! tasks, with their ordinary delegated authority and durable effect records.
use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use shenko_compat::installation::UnitRef;
use shenko_compat_poppy::{
    discovery,
    session::SessionIdentity,
    ucp::{NegotiatedProfile, Transport},
};
use shenko_node::{Node, UnitRequest};

use super::{
    account, commerce,
    connection::{Connection, Operator, OperatorConfig},
    transport::{Authentication, CompanyClient},
    vault::Vault,
};

pub(crate) async fn command(
    data_dir: &Path,
    command: crate::cli::CompatPoppyCommand,
) -> Result<()> {
    let value = match command {
        crate::cli::CompatPoppyCommand::Discover { origin } => {
            serde_json::to_value(discovery::discover(&origin).await?)?
        }
        crate::cli::CompatPoppyCommand::Init {
            client_id,
            jwks_uri,
            profile_uri,
            client_name,
            redirect_uri,
            payment_handlers_file,
        } => {
            crate::runtime_bootstrap::create_data_directory(data_dir)?;
            Operator::initialize(
                data_dir,
                OperatorConfig {
                    schema_version: 1,
                    client_name,
                    client_id,
                    jwks_uri,
                    profile_uri,
                    redirect_uris: redirect_uri,
                    payment_handlers: payment_handlers_file
                        .as_deref()
                        .map(payment_handlers)
                        .transpose()?
                        .unwrap_or_default(),
                },
            )?
            .public_documents()?
        }
        crate::cli::CompatPoppyCommand::ConfigurePayments { file } => {
            Operator::configure_payments(&Vault::open(data_dir, false)?, payment_handlers(&file)?)?
                .public_documents()?
        }
        crate::cli::CompatPoppyCommand::Identity { public_dir } => {
            let operator = Operator::load(&Vault::open(data_dir, false)?)?;
            let documents = operator.public_documents()?;
            if let Some(directory) = public_dir {
                std::fs::create_dir_all(&directory)?;
                for (key, filename) in [
                    ("client_metadata", "client.json"),
                    ("jwks", "jwks.json"),
                    ("ucp_profile", "ucp.json"),
                ] {
                    let temporary = directory.join(format!("public-{}", uuid::Uuid::new_v4()));
                    let bytes = serde_json::to_vec_pretty(&documents[key]["document"])?;
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temporary)?;
                    std::io::Write::write_all(&mut file, &bytes)?;
                    file.sync_all()?;
                    std::fs::rename(temporary, directory.join(filename))?;
                }
                std::fs::File::open(&directory)?.sync_all()?;
            }
            documents
        }
        crate::cli::CompatPoppyCommand::Connect {
            origin,
            unit_id,
            enable,
        } => {
            let report = discovery::discover(&origin).await?;
            let mut node = crate::compat::node_of(data_dir)?;
            let old = match node.installed_unit_fact(&unit_id)? {
                Some(fact) => {
                    ensure!(
                        fact.descriptor
                            .config
                            .pointer("/compat/format")
                            .and_then(Value::as_str)
                            == Some("poppy-connection"),
                        "requested unit identity belongs to another installation format"
                    );
                    let content = node.unit_content(
                        &fact.unit_id,
                        &fact.revision,
                        Some(&fact.artifact_digest),
                    )?;
                    Some(serde_json::from_slice::<Connection>(&content.bytes)?)
                }
                None => None,
            };
            let mut connection = Connection::new(report, &node)?;
            if let Some(old) = old
                && old.report.company_identifier == connection.report.company_identifier
            {
                connection.user_id = old.user_id;
                if old.report.request_protocol_version == connection.report.request_protocol_version
                    && shenko_compat_poppy::transport::session_extensions(&old.report)
                        == shenko_compat_poppy::transport::session_extensions(&connection.report)
                {
                    connection.session_id = old.session_id;
                }
                connection.agent_id = old.agent_id;
            }
            let outcome = node.handle_unit_request(UnitRequest::Import {
                export: connection.export(&unit_id, enable)?,
                package_root: data_dir.join("compat-units").display().to_string(),
                enable: Some(enable),
            })?;
            json!({"installation":outcome,"connection":reference(&node,&unit_id)?,
                "company":connection.report.company_identifier,"user_id":connection.user_id,
                "session_id":connection.session_id,"account_authorization_created":false})
        }
        crate::cli::CompatPoppyCommand::Connections => {
            let mut connections = Vec::new();
            if let Some(node) = Node::open_inspect(&data_dir.join("node"))? {
                for fact in node.installed_unit_facts()? {
                    if fact
                        .descriptor
                        .config
                        .pointer("/compat/format")
                        .and_then(Value::as_str)
                        == Some("poppy-connection")
                    {
                        let bytes = node.unit_content(
                            &fact.unit_id,
                            &fact.revision,
                            Some(&fact.artifact_digest),
                        )?;
                        let connection: Connection = serde_json::from_slice(&bytes.bytes)?;
                        connections.push(json!({"connection":reference(&node,&fact.unit_id)?,"enabled":fact.enabled,
                            "origin":connection.report.company_origin,"company":connection.report.company_identifier,
                            "user_id":connection.user_id,"session_id":connection.session_id}));
                    }
                }
            }
            json!({"connections":connections})
        }
        crate::cli::CompatPoppyCommand::NewSession { unit_id } => {
            let mut node = crate::compat::node_of(data_dir)?;
            let unit = reference(&node, &unit_id)?;
            let (fact, bytes) = unit.exact(&node)?;
            ensure!(
                fact.descriptor
                    .config
                    .pointer("/compat/format")
                    .and_then(Value::as_str)
                    == Some("poppy-connection"),
                "not a Poppy connection"
            );
            let mut connection: Connection = serde_json::from_slice(&bytes)?;
            let identity =
                SessionIdentity::new(&connection.report.company_identifier, &connection.user_id)?;
            let previous =
                std::mem::replace(&mut connection.session_id, identity.session_id().into());
            node.handle_unit_request(UnitRequest::Import {
                export: connection.export(&unit_id, fact.enabled)?,
                package_root: data_dir.join("compat-units").display().to_string(),
                enable: Some(fact.enabled),
            })?;
            json!({"connection":reference(&node,&unit_id)?,"previous_session_id":previous,"session_id":connection.session_id,
                "state_carried_from_old_session":false,"refresh_tokens_changed":false})
        }
        crate::cli::CompatPoppyCommand::Callback { unit_id, file } => {
            let node = crate::compat::node_of(data_dir)?;
            let connection = Connection::read(&node, &reference(&node, &unit_id)?)?;
            let bytes = super::vault::read_private(&file)?;
            let callback = std::str::from_utf8(&bytes)
                .context("callback file is not UTF-8")?
                .trim();
            let vault = Vault::open(data_dir, false)?;
            let reference = account::store_callback(&vault, &connection, callback)?;
            json!({"callback_ref":reference,"secret_exported":false})
        }
        crate::cli::CompatPoppyCommand::Credential {
            reference,
            file,
            unit_id,
            checkout_observation,
            resource,
            signed_out,
        } => {
            let instrument_id = uuid::Uuid::parse_str(
                reference
                    .strip_prefix("instrument-")
                    .context("instrument reference must be instrument-<UUID>")?,
            )
            .context("instrument reference must be instrument-<UUID>")?;
            ensure!(
                reference == format!("instrument-{instrument_id}"),
                "instrument reference must be canonical"
            );
            let bytes = super::vault::read_private(&file)?;
            let instrument: Value =
                serde_json::from_slice(&bytes).context("instrument file is not JSON")?;
            ensure!(
                instrument.is_object(),
                "instrument credential must be an object"
            );
            let node = crate::compat::node_of(data_dir)?;
            let connection = Connection::read(&node, &self::reference(&node, &unit_id)?)?;
            ensure!(
                !signed_out || resource.is_none(),
                "a signed-out credential binding cannot select an account token resource"
            );
            let authentication = if signed_out {
                Authentication::SignedOut
            } else {
                Authentication::Account { resource }
            };
            let client = CompanyClient::new(
                Vault::open(data_dir, false)?,
                connection,
                &authentication,
                Duration::from_secs(60),
            )?;
            let store = crate::compat::store_of(data_dir)?;
            let mut record = commerce::payment_credential_record(
                &client,
                &store,
                checkout_observation,
                instrument,
            )?;
            let encoded = serde_json::to_vec(&record);
            super::transport::clear_json(&mut record);
            let encoded = zeroize::Zeroizing::new(encoded?);
            client.vault().write_immutable(&reference, &encoded)?;
            json!({"credential_ref":reference,"checkout_observation":checkout_observation,
                "node_local":true,"secret_exported":false})
        }
        crate::cli::CompatPoppyCommand::Probe { unit_id } => {
            let node = crate::compat::node_of(data_dir)?;
            let reference = reference(&node, &unit_id)?;
            let connection = Connection::read(&node, &reference)?;
            let guard_node = std::sync::Arc::new(parking_lot::Mutex::new(node));
            let guard_reference = reference.clone();
            let client = CompanyClient::new(
                Vault::open(data_dir, false)?,
                connection,
                &Authentication::SignedOut,
                Duration::from_secs(60),
            )?
            .with_guard(std::sync::Arc::new(move || {
                // This explicit CLI diagnostic owns read-only access while
                // its exact connection remains installed and enabled.
                Connection::read(&guard_node.lock(), &guard_reference)?;
                Ok(())
            }));
            match probe(&client, &reference).await {
                Ok(mut observation) => {
                    client.redact_observation(&mut observation)?;
                    observation
                }
                Err(error) => {
                    let mut diagnostic = json!({"error":format!("{error:#}")});
                    client.redact_observation(&mut diagnostic)?;
                    return Err(error);
                }
            }
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn probe(client: &CompanyClient, reference: &UnitRef) -> Result<Value> {
    // Local export is not deployment: verify all three actual public documents.
    let expected = client.operator().public_documents()?;
    for name in ["client_metadata", "jwks", "ucp_profile"] {
        let url = expected[name]["url"]
            .as_str()
            .context("public document URL missing")?;
        let actual = client.fetch_schema(url).await?;
        ensure!(
            actual == expected[name]["document"],
            "published {name} does not match this node's public identity"
        );
    }
    let business = &client
        .connection()
        .report
        .ucp
        .as_ref()
        .context("company has no UCP profile")?
        .metadata;
    let negotiated = NegotiatedProfile::from_business(business, client.profile())?;
    let service = negotiated.service(Transport::Mcp)?;
    let mut session = client
        .connect_mcp(
            &service.endpoint,
            BTreeMap::from([("UCP-Agent".into(), client.profile().agent_header())]),
        )
        .await?;
    let observed = async {
        let server = session.server_info()?;
        let request = serde_json::from_value(json!({"method":"tools/list","params":{}}))?;
        let result = session
            .request(
                request,
                Duration::from_secs(60),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        Ok::<Value, anyhow::Error>(json!({"connection":reference,
            "published_identity_verified":true,"declared_intersection":negotiated,
            "server":server,"tools_observation":result,
            "account_access_attempted":false,"commerce_mutation_attempted":false}))
    }
    .await;
    let closed = session.close().await;
    let mut value = observed?;
    value["connection_closed"] = closed.is_ok().into();
    Ok(value)
}

fn payment_handlers(path: &Path) -> Result<BTreeMap<String, Vec<Value>>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 64 * 1024,
        "payment handler declarations exceed 64 KiB"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn reference(node: &Node, unit_id: &str) -> Result<UnitRef> {
    let fact = node
        .installed_unit_fact(unit_id)?
        .context("connection unit is not installed")?;
    Ok(UnitRef {
        unit_id: fact.unit_id,
        revision: fact.revision,
        artifact_digest: fact.artifact_digest,
    })
}
