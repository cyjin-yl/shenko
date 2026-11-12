//! Runtime quota observation and notification — docs/specs/quota-resources.md#spec-42-2, docs/specs/quota-resources.md#spec-42-4, docs/specs/quota-resources.md#spec-42-6.
//!
//! This is the runtime half of the quota plugin family. The observers
//! themselves live in [`shenko_quota`] and never perform I/O; this module
//! owns what surrounds them:
//!
//! * **Registration.** A named provider's plugin is installed from the
//!   node's local account configuration. A provider that is not configured
//!   has no plugin, and no observer is assumed to exist.
//! * **Collection.** The runtime reads the real source payload — a statusline
//!   document a CLI session wrote, or a management API document fetched with
//!   the account's own credential — and hands it to the observer. A failed
//!   read becomes an *unavailable* observation, never a balance, and never a
//!   paid probe: no extra generation is ever started to look for a quota.
//! * **Notification.** The durable quota facts become a bounded,
//!   source-named view the subject can read, merged by significant change
//!   so a repeated reading does not fill the subject's context.
//!
//! Quota observation never occupies a model inference permit: it is a
//! background service of the runtime, not a step of the subject loop.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use shenko_quota::{
    AbsentReason, AccountBinding, AccountConfig, Choice, EndpointKind, Freshness, ObservationError,
    ObservationInput, ProviderQuotaPlugin, QuotaEvent, QuotaEventKind, QuotaObserver,
    QuotaPluginRegistry, QuotaScopeView, QuotaSnapshot, SourceKind, MANAGEMENT_LIMITS_CONTRACT,
    MANAGEMENT_USAGE_CONTRACT,
};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use shenko_types::inbox::{InboxEvent, InboxEventKind};

/// The environment variable that names the local account configuration.
///
/// The file holds one [`AccountConfig`]: the named providers this node can
/// observe, the real scope each account is, and the local secret binding
/// used to read account-scoped management data. It is local configuration
/// and never enters the ledger.
pub const QUOTA_ACCOUNTS_ENV: &str = "SHENKO_QUOTA_ACCOUNTS";

/// The inbox event kind quota facts arrive under.
pub const QUOTA_EVENT_KIND: &str = "quota_event";

/// The inbox source quota facts arrive from.
pub const QUOTA_EVENT_SOURCE: &str = "quota";

/// The interval between two collection passes. An account's own refresh
/// interval decides when it is actually read.
const COLLECTION_TICK: Duration = Duration::from_secs(30);

/// How many quota events one publication batch reads.
const EVENT_BATCH: usize = 128;

/// How many batches one publication pass reads. The remainder is picked up
/// by the next pass: a burst never hides a scope's newest event and never
/// starves a scope.
const PUBLISH_BATCHES: usize = 64;

/// Why the runtime could not do its work. Every variant is a bounded
/// classification: no URL, no response body and no credential ever reaches
/// the ledger through this error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaRuntimeError {
    /// The local account configuration could not be read or parsed.
    AccountConfig { path: String, reason: String },
    /// The ledger refused a quota write.
    Store(String),
}

impl std::fmt::Display for QuotaRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaRuntimeError::AccountConfig { path, reason } => {
                write!(f, "quota account configuration {path} unusable: {reason}")
            }
            QuotaRuntimeError::Store(reason) => write!(f, "quota store refused the write: {reason}"),
        }
    }
}

impl std::error::Error for QuotaRuntimeError {}

/// Why one source read failed, before it becomes an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadFailure {
    Unauthorized,
    RateLimited,
    Timeout,
    Transport,
    InvalidData,
}

impl ReadFailure {
    fn observation_error(self) -> ObservationError {
        match self {
            ReadFailure::Unauthorized => ObservationError::Unauthorized,
            ReadFailure::RateLimited => ObservationError::RateLimited,
            ReadFailure::Timeout => ObservationError::Timeout,
            ReadFailure::Transport => ObservationError::Transport,
            ReadFailure::InvalidData => ObservationError::InvalidData,
        }
    }
}

/// The runtime state that is not part of the ledger.
#[derive(Default)]
struct RuntimeState {
    /// When each account scope may be read again.
    due: HashMap<String, Option<DateTime<Utc>>>,
    /// The material content this runtime last published for a scope. It is
    /// the comparison point for merging repeated notifications.
    published: HashMap<String, String>,
    /// The newest quota event position this runtime published from.
    event_seq: u64,
}

/// The runtime that observes provider quota and tells the subject about it.
pub struct QuotaRuntime {
    store: Arc<Store>,
    registry: QuotaPluginRegistry,
    accounts: AccountConfig,
    agent_id: EntityId,
    inbox: Option<mpsc::Sender<InboxEvent>>,
    http: Option<reqwest::Client>,
    state: tokio::sync::Mutex<RuntimeState>,
}

impl QuotaRuntime {
    /// Build the runtime and install the observers of every configured
    /// named provider.
    pub fn new(
        store: Arc<Store>,
        accounts: AccountConfig,
        agent_id: EntityId,
        inbox: Option<mpsc::Sender<InboxEvent>>,
    ) -> Self {
        let mut registry = QuotaPluginRegistry::new();
        // One plugin per named provider: the provider's observers are
        // contributed for the account scopes this node is bound to.
        let mut providers: BTreeMap<String, Vec<AccountBinding>> = BTreeMap::new();
        for account in &accounts.accounts {
            providers
                .entry(account.provider_id.clone())
                .or_default()
                .push(account.clone());
        }
        for (provider_id, bindings) in providers {
            if shenko_quota::sources::contracts_for_provider(&provider_id).is_empty() {
                // A provider this build does not know contributes no
                // observer. It is never approximated by a known provider.
                warn!(provider = %provider_id, "no quota source contract is known for this provider");
                continue;
            }
            let plugin = ProviderQuotaPlugin::new(provider_id.clone(), bindings);
            match registry.register_plugin(Box::new(plugin)) {
                Ok(()) => info!(provider=%provider_id, "quota observers installed"),
                Err(error) => warn!(provider=%provider_id, %error, "quota observer plugin refused"),
            }
        }
        // The management client is only used for account-scoped reads. A
        // client that cannot be built leaves those reads unavailable; it
        // never affects ordinary traffic observation.
        let http = match reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => Some(client),
            Err(error) => {
                warn!(%error, "quota management client unavailable; management reads stay unavailable");
                None
            }
        };
        Self {
            store,
            registry,
            accounts,
            agent_id,
            inbox,
            http,
            state: tokio::sync::Mutex::new(RuntimeState::default()),
        }
    }

    /// Build the runtime from the local account configuration named by the
    /// environment.
    ///
    /// No configuration means no observer and no collection: the runtime
    /// does not exist. A configuration that cannot be read or parsed is an
    /// error, never a silent "no accounts" — a half-read account list would
    /// observe the wrong scope.
    pub fn from_env(
        store: Arc<Store>,
        agent_id: EntityId,
        inbox: Option<mpsc::Sender<InboxEvent>>,
    ) -> Result<Option<Self>, QuotaRuntimeError> {
        let Ok(path) = std::env::var(QUOTA_ACCOUNTS_ENV) else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(&path).map_err(|error| QuotaRuntimeError::AccountConfig {
            path: path.clone(),
            reason: error.to_string(),
        })?;
        let accounts = AccountConfig::from_json(&text).map_err(|error| {
            QuotaRuntimeError::AccountConfig { path: path.clone(), reason: error.to_string() }
        })?;
        Ok(Some(Self::new(store, accounts, agent_id, inbox)))
    }

    /// The providers that currently have an observer installed.
    pub fn providers(&self) -> Vec<String> {
        self.registry.providers()
    }

    /// Every installed observer.
    pub fn observers(&self) -> impl Iterator<Item = &dyn QuotaObserver> {
        self.registry.observers()
    }

    /// The current view of one account scope, if this node has a reading
    /// for it.
    pub fn scope_view(
        &self,
        provider_id: &str,
        account_scope_ref: &str,
    ) -> Result<Option<QuotaScopeView>, QuotaRuntimeError> {
        let Some(snapshot) = self
            .store
            .latest_quota_snapshot(provider_id, account_scope_ref)
            .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?
        else {
            return Ok(None);
        };
        Ok(Some(self.view_for(&snapshot)?))
    }

    /// Read every account whose refresh interval elapsed and record the
    /// resulting observations.
    pub async fn collect_due(&self) -> Result<usize, QuotaRuntimeError> {
        let now = Utc::now();
        let due: Vec<AccountBinding> = {
            let mut state = self.state.lock().await;
            let mut due = Vec::new();
            for account in &self.accounts.accounts {
                let key = account.scope.scope_ref.clone();
                let ready = state.due.get(&key).copied().flatten().is_none_or(|at| at <= now);
                if ready {
                    let interval = chrono::Duration::from_std(account.refresh_interval())
                        .unwrap_or_else(|_| chrono::Duration::minutes(5));
                    state.due.insert(key, Some(now + interval));
                    due.push(account.clone());
                }
            }
            due
        };
        let mut recorded = 0usize;
        for account in due {
            for snapshot in self.collect_account(&account).await {
                match self.store.record_quota_snapshot(&snapshot) {
                    Ok(_) => recorded += 1,
                    Err(error) => warn!(
                        %error,
                        provider = %account.provider_id,
                        scope = %account.scope.scope_ref,
                        "quota observation rejected"
                    ),
                }
            }
        }
        Ok(recorded)
    }

    /// Record that a reading passed the lifetime its policy gives it.
    ///
    /// A stale reading may no longer plan. The fact is recorded once per
    /// reading, so the subject hears about it once instead of on every
    /// pass, and a fresh reading of the same scope supersedes it.
    pub async fn record_stale_readings(&self) -> Result<usize, QuotaRuntimeError> {
        let Some(policy) = self
            .store
            .latest_quota_policy()
            .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?
        else {
            return Ok(0);
        };
        // The scopes the policy itself binds: a reading of another scope is
        // not this policy's evidence.
        let mut scopes: BTreeMap<(String, String), ()> = BTreeMap::new();
        for profile in &policy.profiles {
            for cost in &profile.cost_per_request {
                let key = (cost.window.provider_id.clone(), cost.window.account_scope_ref.clone());
                scopes.insert(key, ());
            }
        }
        let now = Utc::now();
        let mut recorded = 0usize;
        for (provider_id, scope_ref) in scopes.into_keys() {
            let Some(snapshot) = self
                .store
                .latest_quota_snapshot(&provider_id, &scope_ref)
                .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?
            else {
                continue;
            };
            let Some(entry) =
                policy.source_admission(snapshot.source_kind, &snapshot.source_contract_ref)
            else {
                // The policy does not admit this reading: it is not this
                // policy's evidence and it never becomes stale evidence.
                continue;
            };
            if snapshot.freshness(now, entry.options().ttl) != Freshness::Stale {
                continue;
            }
            let last = self
                .store
                .latest_quota_event(&provider_id, &scope_ref, QuotaEventKind::ObservationStale)
                .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?;
            let snapshot_id = snapshot.snapshot_id.to_string();
            let already = last
                .as_ref()
                .and_then(|event| event.payload.get("snapshot_id").and_then(|value| value.as_str()))
                == Some(snapshot_id.as_str());
            if already {
                continue;
            }
            let event = QuotaEvent::new(
                provider_id.clone(),
                scope_ref.clone(),
                QuotaEventKind::ObservationStale,
                serde_json::json!({
                    "snapshot_id": snapshot.snapshot_id,
                    "source_kind": snapshot.source_kind,
                    "source_contract_ref": snapshot.source_contract_ref,
                    "observed_at": snapshot.observed_at,
                    "ttl": entry.options().ttl,
                    "stale_use": entry.options().stale,
                }),
                now,
            );
            match self.store.record_quota_event(&event) {
                Ok(_) => recorded += 1,
                Err(error) => warn!(
                    %error,
                    provider = %provider_id,
                    scope = %scope_ref,
                    "stale quota observation not recorded"
                ),
            }
        }
        Ok(recorded)
    }

    /// Read one account's configured sources.
    async fn collect_account(&self, account: &AccountBinding) -> Vec<QuotaSnapshot> {
        let mut snapshots = Vec::new();
        for endpoint in &account.endpoints {
            let Some(observer) = self.registry.for_scope(
                &account.provider_id,
                &endpoint.source_contract_ref,
                &account.scope.scope_ref,
            ) else {
                // No observer is installed for this contract on this
                // account: nothing is read and nothing is assumed.
                continue;
            };
            let input = match &endpoint.endpoint {
                EndpointKind::StatuslineFile { path } => match std::fs::read_to_string(path) {
                    Ok(json) => ObservationInput::StatuslineJson {
                        json,
                        first_response_seen: self.first_response_seen(account),
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // The CLI session has not written a document yet.
                        // Nothing was observed, so nothing is recorded: an
                        // absent document is not a balance.
                        debug!(
                            provider = %account.provider_id,
                            scope = %account.scope.scope_ref,
                            path = %path,
                            "quota source produced no document yet"
                        );
                        continue;
                    }
                    Err(error) => {
                        warn!(
                            %error,
                            provider = %account.provider_id,
                            path = %path,
                            "quota source document unreadable"
                        );
                        ObservationInput::Unavailable {
                            error: ObservationError::Transport,
                            reason: AbsentReason::Unknown,
                        }
                    }
                },
                EndpointKind::ManagementApi { path } => match self.fetch_management(account, path).await {
                    Ok(body) => match endpoint.source_contract_ref.as_str() {
                        MANAGEMENT_LIMITS_CONTRACT => ObservationInput::ManagementLimits { json: body },
                        MANAGEMENT_USAGE_CONTRACT => ObservationInput::ManagementUsage { json: body },
                        _ => continue,
                    },
                    Err(failure) => {
                        warn!(
                            provider = %account.provider_id,
                            scope = %account.scope.scope_ref,
                            ?failure,
                            "quota management read failed"
                        );
                        ObservationInput::Unavailable {
                            error: failure.observation_error(),
                            reason: AbsentReason::Unknown,
                        }
                    }
                },
            };
            match observer.observe(&input) {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(error) => warn!(
                    %error,
                    provider = %account.provider_id,
                    contract = %endpoint.source_contract_ref,
                    "quota observer refused the payload"
                ),
            }
        }
        snapshots
    }

    /// Whether this account has produced an API response in this ledger.
    ///
    /// The statusline's `rate_limits` block only exists after the account's
    /// first API response, so a missing window may still mean "not yet
    /// observed". The ledger's own traffic is the only evidence this node
    /// has: traffic of other clients stays invisible, which is exactly why
    /// a missing window never becomes a balance.
    fn first_response_seen(&self, account: &AccountBinding) -> bool {
        let attempts = self
            .store
            .quota_attempts(&account.provider_id, &account.scope.scope_ref)
            .unwrap_or_default();
        if !attempts.is_empty() {
            return true;
        }
        self.store
            .latest_quota_snapshot_for_source(
                &account.provider_id,
                &account.scope.scope_ref,
                SourceKind::ApiHeaders,
            )
            .ok()
            .flatten()
            .is_some()
    }

    /// Read one management API document with the account's own credential.
    ///
    /// The credential is resolved at the moment of the read and never
    /// stored: it is not part of the URL, not part of the recorded evidence
    /// and not part of the error.
    async fn fetch_management(
        &self,
        account: &AccountBinding,
        path: &str,
    ) -> Result<String, ReadFailure> {
        let Some(base) = account.management_base_url.as_ref() else {
            return Err(ReadFailure::InvalidData);
        };
        let Some(secret) = account.secret.as_ref() else {
            return Err(ReadFailure::Unauthorized);
        };
        let secret = secret.resolve().map_err(|_| ReadFailure::Unauthorized)?;
        let Some(client) = self.http.as_ref() else {
            return Err(ReadFailure::Transport);
        };
        let url = format!("{}{}", base.trim_end_matches('/'), path);
        if reqwest::Url::parse(&url).is_err() {
            return Err(ReadFailure::InvalidData);
        }
        let mut request = client.get(url.as_str()).header("x-api-key", secret.as_str());
        if let Some(version) = account.api_version.as_ref().filter(|value| !value.trim().is_empty()) {
            request = request.header("anthropic-version", version.as_str());
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() { ReadFailure::Timeout } else { ReadFailure::Transport }
        })?;
        let status = response.status().as_u16();
        match status {
            401 | 403 => return Err(ReadFailure::Unauthorized),
            429 => return Err(ReadFailure::RateLimited),
            200..=299 => {}
            _ => return Err(ReadFailure::InvalidData),
        }
        response.text().await.map_err(|_| ReadFailure::Transport)
    }

    /// Publish the quota facts the subject has not seen yet.
    ///
    /// Several events of one account scope merge into one notification: the
    /// subject reads the newest reading of the scope, not every event.
    ///
    /// The event stream is read in batches, and a pass keeps reading while
    /// batches remain: a burst of events must not hide a scope's newest
    /// event behind the batch limit. The number of batches per pass is
    /// bounded; whatever is left over is picked up by the next pass, so no
    /// scope starves and no event is skipped.
    pub async fn publish(&self) -> Result<usize, QuotaRuntimeError> {
        let mut published = 0usize;
        for _ in 0..PUBLISH_BATCHES {
            let since = self.state.lock().await.event_seq;
            let events = self
                .store
                .quota_events_since(since, EVENT_BATCH)
                .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?;
            if events.is_empty() {
                break;
            }
            let mut newest: BTreeMap<(String, String), QuotaEvent> = BTreeMap::new();
            let mut last_seq = since;
            for (seq, event) in events {
                last_seq = seq;
                newest.insert(
                    (event.provider_id.clone(), event.account_scope_ref.clone()),
                    event,
                );
            }
            for ((provider_id, scope_ref), event) in newest {
                match self.publish_scope(&provider_id, &scope_ref, &event).await {
                    Ok(true) => published += 1,
                    Ok(false) => {}
                    Err(error) => warn!(
                        %error,
                        provider = %provider_id,
                        scope = %scope_ref,
                        "quota notification failed"
                    ),
                }
            }
            self.state.lock().await.event_seq = last_seq;
        }
        Ok(published)
    }

    /// Publish one account scope's current view, unless it did not change.
    async fn publish_scope(
        &self,
        provider_id: &str,
        scope_ref: &str,
        event: &QuotaEvent,
    ) -> Result<bool, QuotaRuntimeError> {
        let Some(snapshot) = self
            .store
            .latest_quota_snapshot(provider_id, scope_ref)
            .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?
        else {
            // An event without a recorded reading (a policy change, for
            // example) has no reading to show. The durable event itself
            // stays queryable in the ledger.
            return Ok(false);
        };
        let view = self.view_for(&snapshot)?;
        let material = view.material();
        let key = format!("{provider_id}/{scope_ref}");
        {
            let state = self.state.lock().await;
            if state.published.get(&key) == Some(&material) {
                return Ok(false);
            }
        }
        let payload = serde_json::json!({
            "text": view.render(),
            "provider_id": provider_id,
            "account_scope_ref": scope_ref,
            "event_kind": event.kind.as_str(),
            "view": view,
        });
        let event_id = stable_event_id(provider_id, scope_ref, &material);
        let inbox_event = InboxEvent {
            event_id,
            kind: InboxEventKind::SystemEvent,
            created_at: Utc::now(),
            source: QUOTA_EVENT_SOURCE.to_string(),
            payload,
        };
        self.store
            .admit_inbox_event(
                event_id,
                self.agent_id,
                QUOTA_EVENT_KIND,
                QUOTA_EVENT_SOURCE,
                &serde_json::to_value(&inbox_event)
                    .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?,
            )
            .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?;
        if let Some(inbox) = self.inbox.as_ref() {
            // This is only a wake. The durable admission above is the fact.
            let _ = inbox.try_send(inbox_event);
        }
        let _ = self.state.lock().await.published.insert(key, material);
        Ok(true)
    }

    /// Build the subject-readable view of one recorded reading.
    fn view_for(&self, snapshot: &QuotaSnapshot) -> Result<QuotaScopeView, QuotaRuntimeError> {
        let policy = self
            .store
            .latest_quota_policy()
            .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?;
        // Freshness is the policy's own lifetime for that exact source: a
        // reading of a contract the policy does not admit has no freshness.
        let freshness = match policy.as_ref() {
            Some(policy) => policy
                .source_admission(snapshot.source_kind, &snapshot.source_contract_ref)
                .map(|entry| snapshot.freshness(Utc::now(), entry.options().ttl))
                .unwrap_or(Freshness::Unknown),
            None => Freshness::Unknown,
        };
        let mut view = QuotaScopeView::from_snapshot(snapshot, freshness);
        if let Some((policy, decision)) = self
            .store
            .latest_quota_decision()
            .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?
        {
            let (profile, model) = match &decision.choice {
                Choice::Cloud { profile_id, .. } | Choice::Local { profile_id } => (
                    Some(profile_id.clone()),
                    policy.model_for_profile(profile_id).map(str::to_owned),
                ),
                _ => (None, None),
            };
            view = view.with_selection(profile, model, Some(choice_reason(&decision.choice)));
            // Which replaceable routing strategy produced this decision
            // (docs/specs/quota-resources.md#spec-42-1, docs/specs/quota-resources.md#spec-42-6): the subject reads it as a fact about the
            // numbers it is looking at, not as a second policy source.
            view = view.with_strategy(decision.strategy.clone());
            // The effort level the decision selected, when the policy
            // supports more than the implicit one (docs/specs/quota-resources.md#spec-42-3).
            let effort = match &decision.choice {
                Choice::Cloud { effort, .. } => effort.clone(),
                _ => None,
            };
            view = view.with_effort(effort);
        }
        // The recorded forecast the decision was planned from: its strategy,
        // horizon, confidence and per-window planning (docs/specs/quota-resources.md#spec-42-3, docs/specs/quota-resources.md#spec-42-6).
        // Projected read-only; the decision itself is never recomputed here.
        if let Some(forecast) = self
            .store
            .latest_quota_forecast(&snapshot.provider_id, &snapshot.account_scope_ref)
            .map_err(|error| QuotaRuntimeError::Store(error.to_string()))?
        {
            view = view.with_forecast(Some(shenko_quota::QuotaForecastView::of(
                &forecast,
                snapshot.snapshot_id,
            )));
        }
        Ok(view)
    }

    /// Run the collection and notification passes until the runtime stops.
    pub async fn run(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        let mut tick = tokio::time::interval(COLLECTION_TICK);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tick.tick() => {
                    if let Err(error) = self.collect_due().await {
                        self.journal_failure("quota_collection", &error);
                    }
                    if let Err(error) = self.record_stale_readings().await {
                        self.journal_failure("quota_staleness", &error);
                    }
                    if let Err(error) = self.publish().await {
                        self.journal_failure("quota_notification", &error);
                    }
                }
            }
        }
    }

    /// Journal one failed pass as a durable runtime fact.
    ///
    /// Quota observation is an optional connector: a failed pass never
    /// stops this consumer and never fails the primary. The failure is
    /// still a fact the subject can read (docs/specs/cognition-units.md#spec-45-14), journaled at this
    /// consumer's own callsite.
    fn journal_failure(&self, kind: &str, error: &QuotaRuntimeError) {
        warn!(kind, %error, "quota pass failed");
        self.store.journal_runtime_error(
            self.agent_id,
            shenko_store::RuntimeErrorContext {
                kind: kind.to_string(),
                source_module: module_path!().into(),
                message: format!("quota pass failed: {error}"),
                step_no: None,
                task_id: None,
                extra: serde_json::json!({}),
            },
        );
    }
}

/// Why the current decision chose what it chose, in the subject's language.
fn choice_reason(choice: &Choice) -> String {
    match choice {
        Choice::Cloud { profile_id, effort, rate_per_hour, .. } => {
            format!(
                "cloud profile {profile_id}, effort={}, rate_per_hour={rate_per_hour:?}",
                effort.as_deref().unwrap_or("implicit"),
            )
        }
        Choice::Local { profile_id } => format!("local profile {profile_id}"),
        Choice::Wait { until } => format!("quota policy wait, next eligible at {until:?}"),
        Choice::ProviderExhausted { windows, until } => {
            format!("provider reports {} window(s) at their limit, until {until:?}", windows.len())
        }
        Choice::ObservationBlocked { windows } => {
            format!("observation blocked for {} window(s)", windows.len())
        }
        Choice::NoBackend => "no configured backend can run".to_string(),
        Choice::CapacityUnknown { profiles } => {
            format!("backend availability unknown for {}", profiles.join(", "))
        }
    }
}

/// A stable event id for one published view. The same reading of the same
/// scope always produces the same id, so a repeated publication is
/// idempotent instead of a second event.
fn stable_event_id(provider_id: &str, scope_ref: &str, material: &str) -> EntityId {
    let identity = format!("quota-view/v1:{provider_id}:{scope_ref}:{material}");
    let hash = blake3::hash(identity.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    EntityId(uuid::Uuid::from_bytes(bytes))
}
