//! The source registry: what reaches the disk, what reaches the vault, and what
//! survives a restart. No database or keychain needed.

use alkyon::model::SourceConfig;
use alkyon::state::{self, AppState};
use alkyon::vault::Vault;
use serde_json::json;

fn config(id: &str) -> SourceConfig {
    serde_json::from_value(json!({
        "id": id,
        "kind": "postgres",
        "host": "db.example.com",
        "database": "warehouse",
        "auth": { "method": "password", "username": "reader", "password": "hunter2" },
    }))
    .expect("the POST /sources wire format")
}

#[tokio::test]
async fn credentials_never_reach_the_sources_file() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), dir.path(), true).unwrap();
    state.register(config("pg")).await.unwrap();

    let raw = std::fs::read_to_string(dir.path().join("sources.json")).unwrap();
    assert!(
        raw.contains(r#""username": "reader""#),
        "the username is not a secret: {raw}"
    );
    assert!(
        !raw.contains("hunter2"),
        "the password reached the disk: {raw}"
    );
    assert!(
        !raw.contains("password\": \""),
        "no password field at all: {raw}"
    );

    // The secret went to the vault instead, under a scope-qualified key.
    assert_eq!(
        state.vault().load("user:pg").unwrap().as_deref(),
        Some("hunter2")
    );
}

#[tokio::test]
async fn sources_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();

    let first = AppState::load(Vault::memory(), dir.path(), true).unwrap();
    first.register(config("pg")).await.unwrap();

    let second = AppState::load(Vault::memory(), dir.path(), true).unwrap();
    let summaries = second.summaries().await;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, "pg");
    assert_eq!(summaries[0].host, "db.example.com");
    assert_eq!(summaries[0].port, 5432, "the default port is filled in");
    assert_eq!(summaries[0].auth_method, "password");
    assert_eq!(summaries[0].dialect, alkyon::model::Dialect::PgSql);
    assert_eq!(summaries[0].editor_mime, "text/x-pgsql");

    // This restart got a fresh memory vault, so the credential is gone — which is
    // exactly the error a user sees if their keychain entry was deleted.
    let Err(error) = second.open("pg", None).await else {
        panic!("opening a source with no stored credential should fail");
    };
    assert!(
        error.to_string().contains("no credential for source `pg`"),
        "unhelpful message: {error}"
    );
}

#[tokio::test]
async fn removing_a_source_clears_both_stores() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), dir.path(), true).unwrap();
    state.register(config("pg")).await.unwrap();

    state.remove("pg").await.unwrap();
    assert_eq!(state.vault().load("pg").unwrap(), None);
    assert!(state.summaries().await.is_empty());
    let raw = std::fs::read_to_string(dir.path().join("sources.json")).unwrap();
    assert_eq!(raw.trim(), "[]");

    let Err(error) = state.remove("pg").await else {
        panic!("removing twice should fail");
    };
    assert_eq!(error.to_string(), "unknown source `pg`");
}

#[tokio::test]
async fn registering_the_same_id_twice_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), dir.path(), true).unwrap();
    state.register(config("pg")).await.unwrap();

    let Err(error) = state.register(config("pg")).await else {
        panic!("a duplicate id should be refused");
    };
    // Qualified, because the clash is within a scope — the same id in the other
    // scope is allowed.
    assert_eq!(error.to_string(), "source `user:pg` already exists");
}

#[tokio::test]
async fn importing_overwrites_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), dir.path(), true).unwrap();
    state.register(config("pg")).await.unwrap();

    // Unlike `register`, an import is allowed to replace what is already there.
    let mut replacement = config("pg");
    replacement.database = Some("staging".into());
    state
        .import(vec![replacement, config("pg2")])
        .await
        .unwrap();

    let summaries = state.summaries().await;
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].database, "staging");

    let reloaded = AppState::load(Vault::memory(), dir.path(), true).unwrap();
    assert_eq!(reloaded.summaries().await.len(), 2);
}

#[tokio::test]
async fn a_missing_sources_file_is_an_empty_registry() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), &dir.path().join("not-created-yet"), true).unwrap();
    assert!(state.summaries().await.is_empty());
}

#[tokio::test]
async fn an_omitted_database_falls_back_per_engine() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), dir.path(), true).unwrap();

    for (kind, expected) in [("postgres", "postgres"), ("ms_sql", "master")] {
        let config: SourceConfig = serde_json::from_value(json!({
            "id": kind,
            "kind": kind,
            "host": "db.example.com",
            "auth": { "method": "password", "username": "reader", "password": "hunter2" },
        }))
        .expect("database may be omitted entirely");
        assert_eq!(config.database(), expected, "{kind}");
        state.register(config).await.unwrap();
    }

    // The API reports the effective database, so the UI never guesses.
    let summaries = state.summaries().await;
    let reported: Vec<&str> = summaries.iter().map(|s| s.database.as_str()).collect();
    assert_eq!(
        reported,
        ["master", "postgres"],
        "sorted by id: ms_sql, postgres"
    );

    // But nothing invented is written down — the field stays absent on disk.
    let raw = std::fs::read_to_string(dir.path().join("sources.json")).unwrap();
    assert!(
        !raw.contains("database"),
        "an unset database is not persisted: {raw}"
    );
}

// ------------------------------------------------------------------- scopes

async fn keys(state: &AppState) -> Vec<String> {
    state
        .summaries()
        .await
        .into_iter()
        .map(|summary| summary.key)
        .collect()
}

fn scoped(id: &str, scope: &str, password: &str) -> SourceConfig {
    serde_json::from_value(json!({
        "id": id,
        "scope": scope,
        "kind": "postgres",
        "host": "db.example.com",
        "auth": { "method": "password", "username": "reader", "password": password },
    }))
    .expect("scope is part of the wire format")
}

#[tokio::test]
async fn the_same_id_can_exist_in_both_scopes() {
    let config = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), config.path(), true).unwrap();
    state
        .open_workspace(project.path().to_str().unwrap())
        .await
        .unwrap();

    state
        .register(scoped("warehouse", "user", "user-pw"))
        .await
        .unwrap();
    state
        .register(scoped("warehouse", "project", "project-pw"))
        .await
        .unwrap();

    assert_eq!(keys(&state).await, ["project:warehouse", "user:warehouse"]);

    // Distinct credentials: a shared vault key would have one silently win.
    assert_eq!(
        state.vault().load("user:warehouse").unwrap().as_deref(),
        Some("user-pw")
    );
    let project_key = format!(
        "project:{}:warehouse",
        project.path().canonicalize().unwrap().to_string_lossy()
    );
    let project_key = project_key.replace(r"\\?\", "");
    assert_eq!(
        state.vault().load(&project_key).unwrap().as_deref(),
        Some("project-pw"),
        "project credentials are keyed by folder, so two projects never collide"
    );

    // A bare id is now ambiguous, and says so instead of guessing.
    let Err(error) = state.record("warehouse").await else {
        panic!("a bare ambiguous id should not resolve");
    };
    assert!(error.to_string().contains("both scopes"), "{error}");

    // Qualified keys still work.
    assert_eq!(
        state.record("user:warehouse").await.unwrap().id,
        "warehouse"
    );
    assert_eq!(
        state.record("project:warehouse").await.unwrap().id,
        "warehouse"
    );
}

#[tokio::test]
async fn each_scope_is_written_to_its_own_file() {
    let config = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), config.path(), true).unwrap();
    state
        .open_workspace(project.path().to_str().unwrap())
        .await
        .unwrap();

    state.register(scoped("mine", "user", "pw")).await.unwrap();
    state
        .register(scoped("ours", "project", "pw"))
        .await
        .unwrap();

    let user = std::fs::read_to_string(config.path().join("sources.json")).unwrap();
    assert!(user.contains("mine") && !user.contains("ours"), "{user}");

    let shared = std::fs::read_to_string(project.path().join(".alkyon/sources.json")).unwrap();
    assert!(
        shared.contains("ours") && !shared.contains("mine"),
        "{shared}"
    );
    // Committable: still no credential, and no scope field duplicating the location.
    assert!(
        !shared.contains("pw") && !shared.contains("scope"),
        "{shared}"
    );
}

#[tokio::test]
async fn project_sources_follow_the_open_folder() {
    let config = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();

    let state = AppState::load(Vault::memory(), config.path(), true).unwrap();
    state.register(scoped("mine", "user", "pw")).await.unwrap();

    state
        .open_workspace(first.path().to_str().unwrap())
        .await
        .unwrap();
    state
        .register(scoped("first-only", "project", "pw"))
        .await
        .unwrap();
    assert_eq!(state.summaries().await.len(), 2);

    // Another folder: its own project sources, and none of the previous ones.
    state
        .open_workspace(second.path().to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        keys(&state).await,
        ["user:mine"],
        "the other folder's sources are gone"
    );

    // Closing drops them too, and the user's survive.
    state
        .open_workspace(first.path().to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        state.summaries().await.len(),
        2,
        "reopening brings them back"
    );
    state.close_workspace().await.unwrap();
    assert_eq!(keys(&state).await, ["user:mine"]);
}

#[tokio::test]
async fn credentials_from_before_scopes_still_open() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), dir.path(), true).unwrap();
    state.register(config("pg")).await.unwrap();

    // Recreate the pre-scope layout: the credential filed under the bare id.
    state.vault().store("pg", "legacy-pw").unwrap();
    state.vault().delete("user:pg").unwrap();

    // Opening fails on the connection, not on a missing credential — proof the
    // legacy entry was found. And it has moved to the scoped key.
    let Err(error) = state.open("pg", None).await else {
        panic!("db.example.com does not exist, so this cannot succeed");
    };
    assert!(
        !error.to_string().contains("no credential"),
        "the legacy credential was not picked up: {error}"
    );
    assert_eq!(
        state.vault().load("user:pg").unwrap().as_deref(),
        Some("legacy-pw"),
        "it should have been moved to the scoped key"
    );
    assert_eq!(
        state.vault().load("pg").unwrap(),
        None,
        "and the old entry cleaned up"
    );
}

#[tokio::test]
async fn a_project_source_needs_a_folder() {
    let config = tempfile::tempdir().unwrap();
    let state = AppState::load(Vault::memory(), config.path(), true).unwrap();

    let Err(error) = state.register(scoped("ours", "project", "pw")).await else {
        panic!("a project source with no folder open should be refused");
    };
    assert!(error.to_string().contains("open folder"), "{error}");
}

#[test]
fn the_terminal_is_loopback_only_by_default() {
    assert!(state::terminal_allowed(&"127.0.0.1:8787".parse().unwrap()));
    assert!(state::terminal_allowed(&"[::1]:8787".parse().unwrap()));
    assert!(
        !state::terminal_allowed(&"0.0.0.0:8787".parse().unwrap()),
        "a wildcard bind would expose an unauthenticated shell"
    );
}
