//! GitHub Issues provider — the only GitHub-specific code in the app.
//!
//! Auth: a token pasted into taisk (kept in the keychain) wins; otherwise
//! the `gh` CLI's own login is reused via `gh auth token`, so a user who
//! already has `gh` set up connects with zero typing. Read-only: the only
//! endpoints used are `GET /user` and `GET /repos/{o}/{r}/issues[/{n}]`.
//!
//! Ticket keys are `owner/repo#number`; containers are `owner/repo`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::Value;

use super::{AuthField, ConnectionStatus, Credentials, ProviderInfo, Ticket, TicketProvider, TicketState};

const ID: &str = "github";
const TOKEN: &str = "token";
/// How long a `gh auth token` answer (including "not logged in") is reused
/// before asking `gh` again — every API call needs a token.
const GH_TOKEN_TTL: Duration = Duration::from_secs(300);

pub struct GithubConfig {
    pub api_base: String,
    /// `gh` binaries to try, in order. A Finder-launched app gets a minimal
    /// PATH, so the usual install locations are listed explicitly too.
    pub gh_candidates: Vec<String>,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            // Overridable so an e2e run can point at a fake GitHub.
            api_base: std::env::var("SESSIONBOARD_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into()),
            gh_candidates: vec!["gh".into(), "/opt/homebrew/bin/gh".into(), "/usr/local/bin/gh".into()],
        }
    }
}

pub struct GithubProvider {
    cfg: GithubConfig,
    http: reqwest::Client,
    /// url → (etag, body). A conditional request answered 304 doesn't count
    /// against the rate limit, so re-listing unchanged repos is free.
    etags: Mutex<HashMap<String, (String, Value)>>,
    gh_token: Mutex<Option<(Instant, Option<String>)>>,
}

impl GithubProvider {
    pub fn new(cfg: GithubConfig) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().expect("reqwest client");
        Self { cfg, http, etags: Mutex::new(HashMap::new()), gh_token: Mutex::new(None) }
    }

    async fn gh_token(&self) -> Option<String> {
        if let Some((at, token)) = self.gh_token.lock().unwrap().as_ref() {
            if at.elapsed() < GH_TOKEN_TTL {
                return token.clone();
            }
        }
        let mut found = None;
        for bin in &self.cfg.gh_candidates {
            let run = tokio::process::Command::new(bin).args(["auth", "token"]).kill_on_drop(true).output();
            if let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(3), run).await {
                let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if out.status.success() && !token.is_empty() {
                    found = Some(token);
                    break;
                }
            }
        }
        *self.gh_token.lock().unwrap() = Some((Instant::now(), found.clone()));
        found
    }

    /// A token pasted into taisk wins over the gh CLI's.
    async fn token(&self, creds: &dyn Credentials) -> Option<(String, &'static str)> {
        if let Some(t) = creds.get(ID, TOKEN) {
            return Some((t, "keychain"));
        }
        self.gh_token().await.map(|t| (t, "gh"))
    }

    async fn get_json(&self, path: &str, token: &str) -> Result<Value, String> {
        let url = format!("{}{path}", self.cfg.api_base);
        let cached_etag = self.etags.lock().unwrap().get(&url).map(|(e, _)| e.clone());
        let mut req = self
            .http
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "taisk");
        if let Some(etag) = &cached_etag {
            req = req.header("If-None-Match", etag);
        }
        let resp = req.send().await.map_err(|e| format!("couldn't reach GitHub: {e}"))?;
        match resp.status() {
            StatusCode::NOT_MODIFIED => {
                self.etags.lock().unwrap().get(&url).map(|(_, v)| v.clone()).ok_or_else(|| "GitHub returned 304 for an uncached request".into())
            }
            s if s.is_success() => {
                let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
                let body: Value = resp.json().await.map_err(|e| format!("unexpected GitHub response: {e}"))?;
                if let Some(etag) = etag {
                    self.etags.lock().unwrap().insert(url, (etag, body.clone()));
                }
                Ok(body)
            }
            StatusCode::UNAUTHORIZED => Err("GitHub rejected the token (401)".into()),
            StatusCode::FORBIDDEN if resp.headers().get("x-ratelimit-remaining").is_some_and(|v| v == "0") => {
                Err("GitHub rate limit reached — try again later".into())
            }
            StatusCode::NOT_FOUND => Err("not found, or the token can't see it (404)".into()),
            s => Err(format!("GitHub returned {s}")),
        }
    }

    async fn viewer(&self, token: &str) -> Result<String, String> {
        let user = self.get_json("/user", token).await?;
        user["login"].as_str().map(str::to_string).ok_or_else(|| "GitHub returned no login".into())
    }

    async fn require_token(&self, creds: &dyn Credentials) -> Result<String, String> {
        self.token(creds).await.map(|(t, _)| t).ok_or_else(|| "GitHub isn't connected".into())
    }
}

/// `owner/repo` from a bare `owner/repo`, an https/ssh GitHub URL (a pasted
/// issue or PR URL works too), or a git remote. `None` for anything else.
pub fn parse_repo(raw: &str) -> Option<String> {
    let s = raw.trim().trim_end_matches('/');
    let (path, is_url) = if let Some(rest) = s.strip_prefix("git@github.com:") {
        (rest, true)
    } else if let Some(i) = s.find("github.com/") {
        (&s[i + "github.com/".len()..], true)
    } else if s.contains(':') || s.contains('@') {
        return None; // some other host
    } else {
        (s, false)
    };
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 2 || (!is_url && parts.len() != 2) {
        return None;
    }
    let owner = parts[0];
    let repo = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
    let ok = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (ok(owner) && ok(repo)).then(|| format!("{owner}/{repo}"))
}

/// `owner/repo#12` → (`owner/repo`, 12).
fn parse_key(key: &str) -> Option<(String, u64)> {
    let (repo, n) = key.rsplit_once('#')?;
    Some((parse_repo(repo)?, n.parse().ok()?))
}

/// The git config governing `cwd`, following a worktree's `.git` file.
fn git_config_for(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return Some(dotgit.join("config"));
        }
        if dotgit.is_file() {
            let raw = std::fs::read_to_string(&dotgit).ok()?;
            let gitdir = dir.join(raw.trim().strip_prefix("gitdir:")?.trim());
            let common = std::fs::read_to_string(gitdir.join("commondir")).ok().map(|c| gitdir.join(c.trim())).unwrap_or(gitdir);
            return Some(common.join("config"));
        }
    }
    None
}

fn github_remotes(config: &str) -> impl Iterator<Item = String> + '_ {
    config.lines().filter_map(|line| {
        let value = line.trim().strip_prefix("url")?.trim_start().strip_prefix('=')?.trim();
        value.contains("github.com").then(|| parse_repo(value)).flatten()
    })
}

fn issue_to_ticket(repo: &str, v: &Value) -> Option<Ticket> {
    if v.get("pull_request").is_some() {
        return None; // the issues API lists PRs too
    }
    let number = v["number"].as_u64()?;
    Some(Ticket {
        provider: ID.into(),
        key: format!("{repo}#{number}"),
        title: v["title"].as_str().unwrap_or_default().to_string(),
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        state: if v["state"] == "closed" { TicketState::Closed } else { TicketState::Open },
        container: repo.to_string(),
        labels: v["labels"].as_array().into_iter().flatten().filter_map(|l| l["name"].as_str().map(str::to_string)).collect(),
        assignee: v["assignee"]["login"].as_str().map(str::to_string),
        updated_at: v["updated_at"].as_str().unwrap_or_default().to_string(),
    })
}

#[async_trait]
impl TicketProvider for GithubProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: ID.into(),
            name: "GitHub".into(),
            container_label: "Repository".into(),
            container_placeholder: "owner/repo".into(),
            auth_fields: vec![AuthField {
                name: TOKEN.into(),
                label: "Personal access token".into(),
                secret: true,
                help: Some(
                    "Fine-grained, read-only: Issues + Metadata. Not needed if the gh CLI is logged in — taisk uses its login."
                        .into(),
                ),
                help_url: Some("https://github.com/settings/personal-access-tokens/new".into()),
            }],
        }
    }

    fn normalize_container(&self, raw: &str) -> Result<String, String> {
        parse_repo(raw).ok_or_else(|| format!("“{}” isn't a GitHub repository (expected owner/repo)", raw.trim()))
    }

    async fn status(&self, creds: &dyn Credentials) -> ConnectionStatus {
        let Some((token, source)) = self.token(creds).await else {
            return ConnectionStatus::default();
        };
        match self.viewer(&token).await {
            Ok(login) => ConnectionStatus { connected: true, account: Some(login), source: Some(source.into()), error: None },
            Err(e) => ConnectionStatus { connected: false, account: None, source: Some(source.into()), error: Some(e) },
        }
    }

    async fn connect(&self, creds: &dyn Credentials, fields: &HashMap<String, String>) -> Result<ConnectionStatus, String> {
        let token = fields.get(TOKEN).map(|t| t.trim()).filter(|t| !t.is_empty()).ok_or("a token is required")?;
        let login = self.viewer(token).await?;
        creds.set(ID, TOKEN, token)?;
        Ok(ConnectionStatus { connected: true, account: Some(login), source: Some("keychain".into()), error: None })
    }

    fn suggest_containers(&self, cwds: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for cwd in cwds {
            let Some(config) = git_config_for(Path::new(cwd)).and_then(|p| std::fs::read_to_string(p).ok()) else { continue };
            for repo in github_remotes(&config) {
                if !out.contains(&repo) {
                    out.push(repo);
                }
            }
        }
        out
    }

    async fn list_open(&self, creds: &dyn Credentials, container: &str) -> Result<Vec<Ticket>, String> {
        let token = self.require_token(creds).await?;
        // First page only (100 most recently updated) — enough to pick from.
        let body = self.get_json(&format!("/repos/{container}/issues?state=open&sort=updated&per_page=100"), &token).await?;
        Ok(body.as_array().into_iter().flatten().filter_map(|v| issue_to_ticket(container, v)).collect())
    }

    async fn get(&self, creds: &dyn Credentials, key: &str) -> Result<Ticket, String> {
        let (repo, number) = parse_key(key).ok_or_else(|| format!("“{key}” isn't a GitHub issue key (owner/repo#number)"))?;
        let token = self.require_token(creds).await?;
        let body = self.get_json(&format!("/repos/{repo}/issues/{number}"), &token).await?;
        issue_to_ticket(&repo, &body).ok_or_else(|| format!("{key} is a pull request, not an issue"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::{Json, Router};

    use super::*;
    use crate::trackers::MemoryCredentials;

    #[test]
    fn parse_repo_accepts_the_usual_spellings_and_rejects_other_hosts() {
        for raw in [
            "omrico94/taisk",
            " omrico94/taisk/ ",
            "https://github.com/omrico94/taisk",
            "https://github.com/omrico94/taisk.git",
            "https://github.com/omrico94/taisk/issues/12",
            "git@github.com:omrico94/taisk.git",
            "ssh://git@github.com/omrico94/taisk.git",
        ] {
            assert_eq!(parse_repo(raw).as_deref(), Some("omrico94/taisk"), "{raw}");
        }
        for raw in ["", "taisk", "a/b/c", "git@gitlab.com:o/r.git", "https://gitlab.com/o/r", "o/r s"] {
            assert_eq!(parse_repo(raw), None, "{raw}");
        }
        assert_eq!(parse_key("o/r#12"), Some(("o/r".into(), 12)));
        assert_eq!(parse_key("o/r#x"), None);
    }

    #[test]
    fn suggests_github_remotes_from_session_dirs_including_worktrees() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("app");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(
            repo.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@github.com:acme/app.git\n[remote \"up\"]\n\turl = https://gitlab.com/x/y\n[remote \"fork\"]\n\turl = https://github.com/me/app\n",
        )
        .unwrap();
        // A linked worktree: `.git` is a file pointing into the main repo.
        let wt = root.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display())).unwrap();
        std::fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();

        let p = GithubProvider::new(GithubConfig::default());
        let cwds = [repo.join("src/deep"), wt, root.path().join("nowhere")].map(|p| p.to_string_lossy().to_string());
        assert_eq!(p.suggest_containers(&cwds), vec!["acme/app".to_string(), "me/app".to_string()]);
    }

    /// A tiny fake of the GitHub REST API: tokens `good`/`from-gh` are valid;
    /// the issues list carries an ETag and answers 304 when it's echoed back.
    async fn mock_github() -> (String, Arc<AtomicUsize>) {
        let full_lists = Arc::new(AtomicUsize::new(0));
        let authed = |h: &HeaderMap| {
            matches!(h.get("authorization").and_then(|v| v.to_str().ok()), Some("Bearer good") | Some("Bearer from-gh"))
        };
        let counter = full_lists.clone();
        let app = Router::new()
            .route(
                "/user",
                get(move |h: HeaderMap| async move {
                    if !authed(&h) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let who = if h["authorization"] == "Bearer good" { "pat-user" } else { "gh-user" };
                    Json(serde_json::json!({"login": who})).into_response()
                }),
            )
            .route(
                "/repos/acme/app/issues",
                get(move |h: HeaderMap| {
                    let counter = counter.clone();
                    async move {
                        if !authed(&h) {
                            return StatusCode::UNAUTHORIZED.into_response();
                        }
                        if h.get("if-none-match").is_some_and(|v| v == "\"v1\"") {
                            return StatusCode::NOT_MODIFIED.into_response();
                        }
                        counter.fetch_add(1, Ordering::SeqCst);
                        let body = serde_json::json!([
                            {"number": 1, "title": "Login broken", "html_url": "https://github.com/acme/app/issues/1", "state": "open",
                             "labels": [{"name": "bug"}], "assignee": {"login": "me"}, "updated_at": "2026-09-01T00:00:00Z"},
                            {"number": 2, "title": "A PR", "html_url": "x", "state": "open", "pull_request": {}, "updated_at": "2026-09-02T00:00:00Z"}
                        ]);
                        ([("etag", "\"v1\"")], Json(body)).into_response()
                    }
                }),
            )
            .route(
                "/repos/acme/app/issues/1",
                get(|| async {
                    Json(serde_json::json!({"number": 1, "title": "Login broken", "html_url": "u", "state": "closed", "updated_at": "z"}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), full_lists)
    }

    fn provider(base: &str, gh: Vec<String>) -> GithubProvider {
        GithubProvider::new(GithubConfig { api_base: base.into(), gh_candidates: gh })
    }

    #[tokio::test]
    async fn lists_issues_without_prs_and_reuses_the_etag() {
        let (base, full_lists) = mock_github().await;
        let p = provider(&base, vec![]);
        let creds = MemoryCredentials::default();
        assert!(p.list_open(&creds, "acme/app").await.unwrap_err().contains("isn't connected"));

        creds.set(ID, TOKEN, "good").unwrap();
        let tickets = p.list_open(&creds, "acme/app").await.unwrap();
        assert_eq!(tickets.len(), 1, "the pull request is filtered out");
        let t = &tickets[0];
        assert_eq!((t.key.as_str(), t.state, t.labels.clone(), t.assignee.as_deref()), ("acme/app#1", TicketState::Open, vec!["bug".to_string()], Some("me")));

        assert_eq!(p.list_open(&creds, "acme/app").await.unwrap(), tickets, "a 304 serves the cached body");
        assert_eq!(full_lists.load(Ordering::SeqCst), 1);

        let one = p.get(&creds, "acme/app#1").await.unwrap();
        assert_eq!(one.state, TicketState::Closed);
        let states = p.states(&creds, &["acme/app#1".into(), "acme/app#999".into()]).await;
        assert_eq!(states, HashMap::from([("acme/app#1".to_string(), TicketState::Closed)]));
    }

    #[tokio::test]
    async fn connect_validates_and_a_pasted_token_wins_over_gh() {
        let (base, _) = mock_github().await;
        let dir = tempfile::tempdir().unwrap();
        let fake_gh = dir.path().join("gh");
        std::fs::write(&fake_gh, "#!/bin/sh\necho from-gh\n").unwrap();
        std::fs::set_permissions(&fake_gh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let p = provider(&base, vec!["/definitely/missing/gh".into(), fake_gh.to_string_lossy().to_string()]);
        let creds = MemoryCredentials::default();
        let s = p.status(&creds).await;
        assert_eq!((s.connected, s.account.as_deref(), s.source.as_deref()), (true, Some("gh-user"), Some("gh")));

        let bad = HashMap::from([(TOKEN.to_string(), "nope".to_string())]);
        assert!(p.connect(&creds, &bad).await.is_err());
        assert_eq!(creds.get(ID, TOKEN), None, "a rejected token is never stored");

        let good = HashMap::from([(TOKEN.to_string(), " good ".to_string())]);
        assert_eq!(p.connect(&creds, &good).await.unwrap().account.as_deref(), Some("pat-user"));
        assert_eq!(p.status(&creds).await.source.as_deref(), Some("keychain"));

        p.disconnect(&creds).await.unwrap();
        assert_eq!(p.status(&creds).await.source.as_deref(), Some("gh"), "falls back to gh after disconnecting");
    }

    #[tokio::test]
    async fn an_invalid_stored_token_reports_an_error_not_a_connection() {
        let (base, _) = mock_github().await;
        let p = provider(&base, vec![]);
        let creds = MemoryCredentials::default();
        assert_eq!(p.status(&creds).await, ConnectionStatus::default());
        creds.set(ID, TOKEN, "expired").unwrap();
        let s = p.status(&creds).await;
        assert!(!s.connected);
        assert!(s.error.unwrap().contains("401"));
    }
}
