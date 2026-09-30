//! Ticket trackers: pull tickets (GitHub issues today; Linear, Jira, … later)
//! onto the board as tasks. Everything in this file is tracker-agnostic — a
//! provider is one `TicketProvider` impl in its own module plus one line in
//! `TrackerRegistry::default_providers`. Tasks, the API routes, the refresher
//! and the frontend only ever see the neutral types below.
//!
//! v1 is read-only: nothing is ever written back to a tracker. Importing a
//! ticket is always an explicit user action (same rule as session→task
//! assignment); the link's open/closed state is refreshed in the background
//! and only ever *shown* — it never moves a task's stage.
//!
//! Secrets (tokens, API keys) go through `Credentials` — the OS keychain in
//! production — and are never written to any file in the app data dir.
//! `trackers.json` only records which containers (repo / team / project) are
//! linked to which board.

pub mod github;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::tasks::{TaskHub, TasksSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketState {
    Open,
    Closed,
}

/// A provider-neutral ticket. `key` is the provider's own human id
/// (`owner/repo#12`, `ENG-42`, `PROJ-7`) and, together with `provider`, is
/// the identity used for dedup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    pub provider: String,
    pub key: String,
    pub title: String,
    pub url: String,
    pub state: TicketState,
    /// Where it lives: a repo, team or project, in the provider's own notation.
    pub container: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    /// RFC 3339, UTC (`…Z`) — lexically sortable across providers.
    #[serde(default)]
    pub updated_at: String,
}

/// What a task keeps about the ticket it was imported from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TicketRef {
    pub provider: String,
    pub key: String,
    pub title: String,
    pub url: String,
    pub state: TicketState,
}

impl From<&Ticket> for TicketRef {
    fn from(t: &Ticket) -> Self {
        Self { provider: t.provider.clone(), key: t.key.clone(), title: t.title.clone(), url: t.url.clone(), state: t.state }
    }
}

/// One input the connect form needs (a token, an email, a site URL, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthField {
    pub name: String,
    pub label: String,
    /// Rendered as a password input and stored only in the keychain.
    pub secret: bool,
    #[serde(default)]
    pub help: Option<String>,
    #[serde(default)]
    pub help_url: Option<String>,
}

/// Everything the UI needs to render a provider without knowing which it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    /// What one linkable unit is called: "Repository", "Team", "Project".
    pub container_label: String,
    pub container_placeholder: String,
    pub auth_fields: Vec<AuthField>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectionStatus {
    pub connected: bool,
    /// Who we're connected as (a login, an email).
    pub account: Option<String>,
    /// Where the credentials came from, e.g. `"keychain"` or `"gh"`.
    pub source: Option<String>,
    /// Why a present credential didn't work (expired token, network, …).
    pub error: Option<String>,
}

/// Per-provider secret storage. Keys are `(provider, field)`.
pub trait Credentials: Send + Sync {
    fn get(&self, provider: &str, field: &str) -> Option<String>;
    fn set(&self, provider: &str, field: &str, value: &str) -> Result<(), String>;
    /// Deleting something that isn't there is not an error.
    fn delete(&self, provider: &str, field: &str) -> Result<(), String>;
}

/// OS keychain (macOS Keychain, Windows Credential Manager, Linux keyutils),
/// service `taisk`, account `<provider>:<field>`.
pub struct KeychainCredentials;

impl KeychainCredentials {
    fn entry(provider: &str, field: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new("taisk", &format!("{provider}:{field}")).map_err(|e| e.to_string())
    }
}

impl Credentials for KeychainCredentials {
    fn get(&self, provider: &str, field: &str) -> Option<String> {
        Self::entry(provider, field).ok()?.get_password().ok().filter(|s| !s.is_empty())
    }

    fn set(&self, provider: &str, field: &str, value: &str) -> Result<(), String> {
        Self::entry(provider, field)?.set_password(value).map_err(|e| format!("couldn't save to the system keychain: {e}"))
    }

    fn delete(&self, provider: &str, field: &str) -> Result<(), String> {
        match Self::entry(provider, field)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Tests and the E2E harness: never touches the real keychain. Nothing
/// survives a restart.
#[derive(Default)]
pub struct MemoryCredentials(Mutex<HashMap<(String, String), String>>);

impl Credentials for MemoryCredentials {
    fn get(&self, provider: &str, field: &str) -> Option<String> {
        self.0.lock().unwrap().get(&(provider.to_string(), field.to_string())).cloned()
    }

    fn set(&self, provider: &str, field: &str, value: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert((provider.to_string(), field.to_string()), value.to_string());
        Ok(())
    }

    fn delete(&self, provider: &str, field: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(&(provider.to_string(), field.to_string()));
        Ok(())
    }
}

/// One ticketing system. Implement this (plus a registry line) to add one.
#[async_trait]
pub trait TicketProvider: Send + Sync {
    fn info(&self) -> ProviderInfo;

    /// Validates and canonicalizes what the user typed as a container (e.g.
    /// a pasted repo URL → `owner/repo`).
    fn normalize_container(&self, raw: &str) -> Result<String, String>;

    async fn status(&self, creds: &dyn Credentials) -> ConnectionStatus;

    /// Validates `fields` against the real service, then stores them.
    async fn connect(&self, creds: &dyn Credentials, fields: &HashMap<String, String>) -> Result<ConnectionStatus, String>;

    /// Forgets every stored auth field.
    async fn disconnect(&self, creds: &dyn Credentials) -> Result<(), String> {
        for field in self.info().auth_fields {
            creds.delete(&self.info().id, &field.name)?;
        }
        Ok(())
    }

    /// Containers worth offering given the working directories of the
    /// board's sessions (e.g. git remotes). Empty when there's no signal.
    fn suggest_containers(&self, _cwds: &[String]) -> Vec<String> {
        Vec::new()
    }

    async fn list_open(&self, creds: &dyn Credentials, container: &str) -> Result<Vec<Ticket>, String>;

    async fn get(&self, creds: &dyn Credentials, key: &str) -> Result<Ticket, String>;

    /// Current state of each key. Keys that fail to resolve are left out
    /// (their tasks just keep the last known state). Providers with a batch
    /// API should override this.
    async fn states(&self, creds: &dyn Credentials, keys: &[String]) -> HashMap<String, TicketState> {
        futures::stream::iter(keys.iter().cloned())
            .map(|key| async move { self.get(creds, &key).await.ok().map(|t| (key, t.state)) })
            .buffer_unordered(4)
            .filter_map(|x| async move { x })
            .collect()
            .await
    }

    /// Initial prompt for the first Claude session started from a task
    /// linked to this ticket. The URL rather than the body keeps argv small;
    /// Claude can fetch the details itself.
    fn session_prompt(&self, t: &TicketRef) -> String {
        format!("Work on {} {}: {}\n\n{}", self.info().name, t.key, t.title, t.url)
    }
}

pub struct TrackerRegistry {
    providers: Vec<Arc<dyn TicketProvider>>,
}

impl TrackerRegistry {
    pub fn new(providers: Vec<Arc<dyn TicketProvider>>) -> Self {
        Self { providers }
    }

    /// Every provider the app ships. Adding a tracker = one line here.
    pub fn default_providers() -> Self {
        Self::new(vec![Arc::new(github::GithubProvider::new(github::GithubConfig::default()))])
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn TicketProvider>> {
        self.providers.iter().find(|p| p.info().id == id).cloned()
    }

    pub fn all(&self) -> &[Arc<dyn TicketProvider>] {
        &self.providers
    }
}

/// A container of some provider, linked to a board.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Link {
    pub provider: String,
    pub container: String,
}

/// board id → linked containers. Flat JSON, same atomic tmp+rename write as
/// `BoardStore`. Holds no secrets.
pub struct TrackerLinks {
    path: Option<PathBuf>,
    inner: RwLock<HashMap<String, Vec<Link>>>,
}

impl TrackerLinks {
    pub fn in_memory() -> Self {
        Self { path: None, inner: RwLock::new(HashMap::new()) }
    }

    pub fn load(path: &Path) -> Self {
        let inner = std::fs::read_to_string(path).ok().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default();
        Self { path: Some(path.to_path_buf()), inner: RwLock::new(inner) }
    }

    pub fn get(&self, board: &str) -> Vec<Link> {
        self.inner.read().unwrap().get(board).cloned().unwrap_or_default()
    }

    /// Replaces the board's links (order kept, duplicates dropped).
    pub fn set(&self, board: &str, links: Vec<Link>) -> std::io::Result<Vec<Link>> {
        let mut seen = HashSet::new();
        let links: Vec<Link> = links.into_iter().filter(|l| seen.insert(l.clone())).collect();
        {
            let mut inner = self.inner.write().unwrap();
            if links.is_empty() {
                inner.remove(board);
            } else {
                inner.insert(board.to_string(), links.clone());
            }
        }
        self.save()?;
        Ok(links)
    }

    pub fn remove_board(&self, board: &str) {
        if self.inner.write().unwrap().remove(board).is_some() {
            let _ = self.save();
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let json = serde_json::to_string_pretty(&*self.inner.read().unwrap())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

/// The keychain, unless `SESSIONBOARD_EPHEMERAL_CREDENTIALS` is set — e2e
/// runs use that so they never read or write the user's real keychain.
pub fn default_credentials() -> Arc<dyn Credentials> {
    if std::env::var_os("SESSIONBOARD_EPHEMERAL_CREDENTIALS").is_some() {
        Arc::new(MemoryCredentials::default())
    } else {
        Arc::new(KeychainCredentials)
    }
}

/// Everything the API needs, shared and cheap to clone.
#[derive(Clone)]
pub struct Trackers {
    pub registry: Arc<TrackerRegistry>,
    pub creds: Arc<dyn Credentials>,
    pub links: Arc<TrackerLinks>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackerOverview {
    #[serde(flatten)]
    pub info: ProviderInfo,
    pub status: ConnectionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketItem {
    #[serde(flatten)]
    pub ticket: Ticket,
    /// The task already linked to this ticket on this board, if any.
    pub imported_task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkError {
    pub provider: String,
    pub container: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TicketsResponse {
    pub tickets: Vec<TicketItem>,
    /// Per-link failures; other links' tickets are still returned.
    pub errors: Vec<LinkError>,
}

impl Trackers {
    pub fn new(registry: TrackerRegistry, creds: Arc<dyn Credentials>, links: TrackerLinks) -> Self {
        Self { registry: Arc::new(registry), creds, links: Arc::new(links) }
    }

    pub async fn overview(&self) -> Vec<TrackerOverview> {
        let mut out = Vec::new();
        for p in self.registry.all() {
            out.push(TrackerOverview { info: p.info(), status: p.status(self.creds.as_ref()).await });
        }
        out
    }

    /// Open tickets across every container linked to `board`, newest first.
    pub async fn open_tickets(&self, board: &str, tasks: &TasksSnapshot) -> TicketsResponse {
        let imported: HashMap<(&str, &str), &str> = tasks
            .tasks
            .iter()
            .filter(|t| t.board == board)
            .filter_map(|t| t.ticket.as_ref().map(|r| ((r.provider.as_str(), r.key.as_str()), t.id.as_str())))
            .collect();
        let mut resp = TicketsResponse::default();
        let fetches = self.links.get(board).into_iter().map(|link| async move {
            let result = match self.registry.get(&link.provider) {
                Some(p) => p.list_open(self.creds.as_ref(), &link.container).await,
                None => Err(format!("unknown tracker `{}`", link.provider)),
            };
            (link, result)
        });
        for (link, result) in futures::future::join_all(fetches).await {
            match result {
                Ok(tickets) => resp.tickets.extend(tickets.into_iter().map(|ticket| TicketItem {
                    imported_task_id: imported.get(&(ticket.provider.as_str(), ticket.key.as_str())).map(|s| s.to_string()),
                    ticket,
                })),
                Err(error) => resp.errors.push(LinkError { provider: link.provider, container: link.container, error }),
            }
        }
        resp.tickets.sort_by(|a, b| b.ticket.updated_at.cmp(&a.ticket.updated_at));
        resp
    }

    /// One refresher pass: re-reads the state of every linked ticket and
    /// applies changes (one batched mutation, so at most one broadcast).
    pub async fn refresh_linked_states(&self, tasks: &TaskHub) {
        let snap = tasks.snapshot().await;
        let mut by_provider: HashMap<String, HashSet<String>> = HashMap::new();
        for r in snap.tasks.iter().filter_map(|t| t.ticket.as_ref()) {
            by_provider.entry(r.provider.clone()).or_default().insert(r.key.clone());
        }
        let mut updates = Vec::new();
        for (provider_id, keys) in by_provider {
            let Some(provider) = self.registry.get(&provider_id) else { continue };
            let keys: Vec<String> = keys.into_iter().collect();
            for (key, state) in provider.states(self.creds.as_ref(), &keys).await {
                updates.push((provider_id.clone(), key, state));
            }
        }
        tasks.set_ticket_states(&updates).await;
    }
}

/// Keeps linked tickets' open/closed state fresh. Spawned from bootstrap.
pub async fn run_ticket_refresher(trackers: Trackers, tasks: TaskHub, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        trackers.refresh_linked_states(&tasks).await;
    }
}

/// `SESSIONBOARD_TICKET_REFRESH_SECS` shortens the interval for manual e2e
/// runs, like the other TTL env vars; default 10 min.
pub fn refresh_interval() -> Duration {
    std::env::var("SESSIONBOARD_TICKET_REFRESH_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(600))
}

#[cfg(test)]
pub mod fake {
    //! An in-memory provider. The generic pipeline (registry, merge, import,
    //! refresher, API) is tested against this, with no provider code at all.
    use super::*;

    pub struct FakeProvider {
        pub id: String,
        pub tickets: Mutex<Vec<Ticket>>,
    }

    impl FakeProvider {
        pub fn new(id: &str) -> Self {
            Self { id: id.to_string(), tickets: Mutex::new(Vec::new()) }
        }

        pub fn ticket(&self, container: &str, key: &str, title: &str, updated_at: &str) -> Ticket {
            let t = Ticket {
                provider: self.id.clone(),
                key: key.to_string(),
                title: title.to_string(),
                url: format!("https://fake.example/{key}"),
                state: TicketState::Open,
                container: container.to_string(),
                labels: vec![],
                assignee: None,
                updated_at: updated_at.to_string(),
            };
            self.tickets.lock().unwrap().push(t.clone());
            t
        }

        pub fn set_state(&self, key: &str, state: TicketState) {
            for t in self.tickets.lock().unwrap().iter_mut().filter(|t| t.key == key) {
                t.state = state;
            }
        }
    }

    #[async_trait]
    impl TicketProvider for FakeProvider {
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                id: self.id.clone(),
                name: "Fake".into(),
                container_label: "Project".into(),
                container_placeholder: "PROJ".into(),
                auth_fields: vec![AuthField { name: "token".into(), label: "Token".into(), secret: true, help: None, help_url: None }],
            }
        }

        fn normalize_container(&self, raw: &str) -> Result<String, String> {
            let raw = raw.trim();
            if raw.is_empty() {
                Err("empty".into())
            } else {
                Ok(raw.to_uppercase())
            }
        }

        async fn status(&self, creds: &dyn Credentials) -> ConnectionStatus {
            match creds.get(&self.id, "token") {
                Some(_) => ConnectionStatus { connected: true, account: Some("me".into()), source: Some("keychain".into()), error: None },
                None => ConnectionStatus::default(),
            }
        }

        async fn connect(&self, creds: &dyn Credentials, fields: &HashMap<String, String>) -> Result<ConnectionStatus, String> {
            match fields.get("token").map(|t| t.trim()) {
                Some("good") => {
                    creds.set(&self.id, "token", "good")?;
                    Ok(self.status(creds).await)
                }
                _ => Err("bad token".into()),
            }
        }

        async fn list_open(&self, _creds: &dyn Credentials, container: &str) -> Result<Vec<Ticket>, String> {
            if container == "BROKEN" {
                return Err("boom".into());
            }
            Ok(self
                .tickets
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.container == container && t.state == TicketState::Open)
                .cloned()
                .collect())
        }

        async fn get(&self, _creds: &dyn Credentials, key: &str) -> Result<Ticket, String> {
            self.tickets.lock().unwrap().iter().find(|t| t.key == key).cloned().ok_or_else(|| "not found".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeProvider;
    use super::*;
    use crate::tasks::Stage;

    fn trackers(p: Arc<FakeProvider>) -> Trackers {
        Trackers::new(TrackerRegistry::new(vec![p]), Arc::new(MemoryCredentials::default()), TrackerLinks::in_memory())
    }

    #[tokio::test]
    async fn open_tickets_merges_links_newest_first_marks_imported_and_reports_broken_links() {
        let p = Arc::new(FakeProvider::new("fake"));
        let older = p.ticket("A", "A-1", "older", "2026-01-01T00:00:00Z");
        p.ticket("B", "B-1", "newer", "2026-02-01T00:00:00Z");
        let tr = trackers(p.clone());
        tr.links
            .set(
                "default",
                vec![
                    Link { provider: "fake".into(), container: "A".into() },
                    Link { provider: "fake".into(), container: "B".into() },
                    Link { provider: "fake".into(), container: "BROKEN".into() },
                    Link { provider: "nope".into(), container: "X".into() },
                ],
            )
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::load(&dir.path().join("tasks.json"));
        let task = hub.create_from_ticket(TicketRef::from(&older), "default", Stage::Todo).await;

        let resp = tr.open_tickets("default", &hub.snapshot().await).await;
        let keys: Vec<&str> = resp.tickets.iter().map(|t| t.ticket.key.as_str()).collect();
        assert_eq!(keys, ["B-1", "A-1"]);
        assert_eq!(resp.tickets[1].imported_task_id.as_deref(), Some(task.id.as_str()));
        assert_eq!(resp.tickets[0].imported_task_id, None);
        assert_eq!(resp.errors.len(), 2);

        // Another board doesn't see this board's import.
        tr.links.set("work", vec![Link { provider: "fake".into(), container: "A".into() }]).unwrap();
        let resp = tr.open_tickets("work", &hub.snapshot().await).await;
        assert_eq!(resp.tickets[0].imported_task_id, None);
    }

    #[tokio::test]
    async fn refresher_applies_state_changes_and_broadcasts_only_on_change() {
        let p = Arc::new(FakeProvider::new("fake"));
        let t = p.ticket("A", "A-1", "x", "");
        let tr = trackers(p.clone());
        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::load(&dir.path().join("tasks.json"));
        hub.create_from_ticket(TicketRef::from(&t), "default", Stage::Todo).await;
        let mut rx = hub.subscribe();

        tr.refresh_linked_states(&hub).await;
        assert!(rx.try_recv().is_err(), "no change, no broadcast");

        p.set_state("A-1", TicketState::Closed);
        tr.refresh_linked_states(&hub).await;
        let snap = rx.try_recv().expect("state change must broadcast");
        assert_eq!(snap.tasks[0].ticket.as_ref().unwrap().state, TicketState::Closed);
        assert_eq!(snap.tasks[0].stage, Stage::Todo, "ticket state never moves the stage");
    }

    #[test]
    fn links_persist_dedup_and_drop_with_their_board() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trackers.json");
        let links = TrackerLinks::load(&path);
        let a = Link { provider: "github".into(), container: "o/r".into() };
        let saved = links.set("work", vec![a.clone(), a.clone()]).unwrap();
        assert_eq!(saved, vec![a.clone()]);
        assert_eq!(TrackerLinks::load(&path).get("work"), vec![a]);
        links.remove_board("work");
        assert!(TrackerLinks::load(&path).get("work").is_empty());
    }

    #[tokio::test]
    async fn default_disconnect_forgets_every_auth_field() {
        let p = FakeProvider::new("fake");
        let creds = MemoryCredentials::default();
        let fields = HashMap::from([("token".to_string(), "good".to_string())]);
        assert!(p.connect(&creds, &fields).await.unwrap().connected);
        p.disconnect(&creds).await.unwrap();
        assert!(!p.status(&creds).await.connected);
    }
}
