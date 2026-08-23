//! How the editor behaves, in two files: the user's own and the open folder's.
//!
//! The same two scopes the source registry has — `settings.json` beside
//! `sources.json` in the config directory, and `.alkyon/settings.json` inside the
//! project, committable and holding no credential.
//!
//! **The folder's file wins, field by field.** Sources never had to answer this
//! question: a source lives in one registry or the other and both are listed, so
//! there is nothing to reconcile. A single record of settings does have to be
//! reconciled, and layering is the only answer that behaves the way anyone
//! expects — *"I want ghost text everywhere"* as a personal preference, *"not in
//! this project"* as an override. Replacing wholesale would mean a project file
//! naming one setting silently reset every other one to the built-in default
//! rather than to what the user asked for.
//!
//! The layering happens on the JSON rather than on the struct, which is what
//! keeps every field a plain `bool` or `u64` instead of an `Option` with a third
//! state to explain in the dialogue.
//!
//! **Read on demand**, not cached. Both files are a few hundred bytes, editing
//! one should take effect on the next keystroke rather than on the next restart,
//! and there is then nothing that can go stale.
//!
//! A file that is not valid JSON is logged and skipped, the way a broken
//! `sources.json` is: a typo in a setting must not be the reason the editor stops
//! completing — and a broken *user* file must not take the project's with it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::model::Scope;

/// In the config directory, beside `sources.json`.
pub const USER_FILE: &str = "settings.json";
/// In the open folder, beside its own source registry.
pub const PROJECT_FILE: &str = ".alkyon/settings.json";

/// Below this, a suggestion is asked for on nearly every keystroke.
const MIN_DEBOUNCE_MS: u64 = 50;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub completion: Completion,
}

/// The two kinds of completion, each switched on its own.
///
/// They are two different things rather than two settings of one, which is why
/// there are two switches. The **dropdown** is CodeMirror's list, built from the
/// schema alkyon already holds: free, offline, and exact — it can only ever
/// offer a name that is really there. **Ghost text** is the greyed suggestion a
/// model writes: it costs a round trip to a third party, it can be wrong, and it
/// can propose the thing you were about to type. Wanting one without the other
/// is the ordinary case, in both directions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Completion {
    pub dropdown: bool,
    pub ghost_text: bool,
    pub model: String,
    /// How long typing has to stop before a suggestion is asked for.
    pub debounce_ms: u64,
    /// How many tables travel with a request *with their columns*. Every other
    /// table in the database still goes as a bare name, which is what lets a
    /// suggestion name a table the buffer has not mentioned yet.
    pub max_tables: usize,
}

impl Default for Completion {
    fn default() -> Self {
        Completion {
            // On: it needs no key, no network and no third party.
            dropdown: true,
            // Off. It sends the schema and the buffer somewhere else, and that is
            // not a thing to start doing because a folder was opened.
            ghost_text: false,
            // Small and fast on purpose — see `llm::MODEL`.
            model: crate::llm::MODEL.to_owned(),
            debounce_ms: 250,
            max_tables: 12,
        }
    }
}

impl Settings {
    /// Refuse the one value that costs money to get wrong.
    ///
    /// A debounce of zero asks for a suggestion on every keystroke, which is a
    /// request per character typed. Clamping it silently would be worse than
    /// refusing: the box would say `0` and mean something else. Nothing else is
    /// checked — a strange `max_tables` costs a shorter prompt and no more,
    /// because the character budget bounds the request whatever it says.
    pub fn checked(self) -> Result<Settings> {
        if self.completion.ghost_text && self.completion.debounce_ms < MIN_DEBOUNCE_MS {
            return Err(Error::BadRequest(format!(
                "a debounce of {} ms asks for a suggestion on nearly every keystroke — \
                 {MIN_DEBOUNCE_MS} ms is the lowest that makes sense",
                self.completion.debounce_ms
            )));
        }
        if self.completion.model.trim().is_empty() {
            return Err(Error::BadRequest("give the model a name".into()));
        }
        Ok(self)
    }
}

/// One of the two places settings can live.
#[derive(Debug, Clone)]
pub struct Layer {
    pub scope: Scope,
    /// Where it is. `None` for the project layer with no folder open, and for
    /// the user layer when there is no config directory to write into.
    pub file: Option<PathBuf>,
    /// What to show. The user's own file is at a path nobody can guess, so it is
    /// shown whole; the project's is always the same name under the open folder,
    /// which reads better short.
    pub shown: String,
    pub present: bool,
}

impl Layer {
    fn of(scope: Scope, file: Option<PathBuf>) -> Layer {
        let present = file.as_deref().is_some_and(Path::exists);
        let shown = match (scope, &file) {
            (Scope::Project, _) => PROJECT_FILE.to_owned(),
            (Scope::User, Some(path)) => path.display().to_string(),
            (Scope::User, None) => USER_FILE.to_owned(),
        };
        Layer {
            scope,
            file,
            shown,
            present,
        }
    }
}

/// Both layers, in the order they are applied.
pub fn layers(user_file: Option<&Path>, project_root: Option<&Path>) -> Vec<Layer> {
    vec![
        Layer::of(Scope::User, user_file.map(Path::to_path_buf)),
        Layer::of(
            Scope::Project,
            project_root.map(|root| root.join(PROJECT_FILE)),
        ),
    ]
}

/// The settings in force: the user's own, with the folder's laid over them.
pub fn load(user_file: Option<&Path>, project_root: Option<&Path>) -> Settings {
    let mut merged = Value::Object(Map::new());
    for layer in layers(user_file, project_root) {
        if let Some(value) = read(layer.file.as_deref()) {
            merge(&mut merged, value);
        }
    }
    serde_json::from_value(merged).unwrap_or_else(|e| {
        tracing::error!(error = %e, "settings have the wrong shape, using the defaults");
        Settings::default()
    })
}

/// One file as JSON, or `None` when it is absent, unreadable or not JSON.
fn read(file: Option<&Path>) -> Option<Value> {
    let file = file?;
    let raw = match std::fs::read_to_string(file) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %file.display(), error = %e, "cannot read settings");
            return None;
        }
    };
    match serde_json::from_str(&raw) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::error!(
                path = %file.display(),
                error = %e,
                "settings file is not valid JSON, skipping it"
            );
            None
        }
    }
}

/// `over` wins, field by field and at every depth.
///
/// Recursive rather than a top-level replace: `{"completion": {"dropdown":
/// false}}` in a project has to override that one switch and leave the model and
/// the debounce the user chose, not reset them.
fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, over) => *base = over,
    }
}

/// Write one layer, creating `.alkyon/` if this is the first thing to live in it.
///
/// Through a temporary file and a rename, like the source registry beside it: a
/// half-written settings file would be read as a broken one on the next
/// keystroke. The trailing newline is for the same reason as there — a project
/// file is committed, and one ending mid-line shows up in every diff that touches
/// it.
///
/// **The whole document is rewritten.** A key nobody here knows about does not
/// survive a save from the dialogue, which is the price of round-tripping through
/// a typed struct rather than through the raw JSON.
pub fn store(file: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = file.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        format!("{}\n", serde_json::to_string_pretty(settings)?),
    )?;
    std::fs::rename(&temporary, file)?;
    tracing::info!(path = %file.display(), "wrote settings");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user file, a project root, and the settings that come out of the pair.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config").join(USER_FILE);
        let project = dir.path().join("project");
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        (dir, user, project)
    }

    fn write(file: &Path, json: &str) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, json).unwrap();
    }

    #[test]
    fn nothing_anywhere_gives_the_defaults() {
        let defaults = load(None, None);
        assert!(defaults.completion.dropdown, "the offline one is on");
        assert!(
            !defaults.completion.ghost_text,
            "the one that leaves the machine is not"
        );
    }

    /// The whole reason the layers merge rather than replace: a project that
    /// turns one switch off must not silently undo the rest of what the user
    /// chose.
    #[test]
    fn the_folder_overrides_one_setting_and_inherits_the_others() {
        let (_dir, user, project) = fixture();
        write(
            &user,
            r#"{"completion": {"ghost_text": true, "debounce_ms": 400, "model": "mine"}}"#,
        );
        write(
            &project.join(PROJECT_FILE),
            r#"{"completion": {"ghost_text": false}}"#,
        );

        let settings = load(Some(&user), Some(&project));
        assert!(!settings.completion.ghost_text, "the folder said no");
        assert_eq!(settings.completion.debounce_ms, 400, "the user's, still");
        assert_eq!(settings.completion.model, "mine", "the user's, still");
    }

    #[test]
    fn a_user_file_alone_is_in_force_everywhere() {
        let (_dir, user, project) = fixture();
        write(&user, r#"{"completion": {"ghost_text": true}}"#);

        for root in [None, Some(project.as_path())] {
            let settings = load(Some(&user), root);
            assert!(settings.completion.ghost_text, "root = {root:?}");
            assert!(settings.completion.dropdown, "and the rest is default");
        }
    }

    /// A broken user file must not take the project's settings down with it.
    #[test]
    fn a_broken_layer_is_skipped_and_the_other_still_applies() {
        let (_dir, user, project) = fixture();
        write(&user, "{ not json");
        write(
            &project.join(PROJECT_FILE),
            r#"{"completion": {"ghost_text": true}}"#,
        );

        let settings = load(Some(&user), Some(&project));
        assert!(settings.completion.ghost_text, "the readable one applied");
        assert!(settings.completion.dropdown);
    }

    /// Saving into a project that has never had a `.alkyon/` — the ordinary case,
    /// since the dialogue is how the file comes to exist at all.
    #[test]
    fn storing_creates_the_directory_and_round_trips() {
        let (_dir, _user, project) = fixture();
        let file = project.join(PROJECT_FILE);
        assert!(!file.parent().unwrap().exists(), "nothing there yet");

        let mut settings = Settings::default();
        settings.completion.ghost_text = true;
        settings.completion.dropdown = false;
        settings.completion.debounce_ms = 300;
        store(&file, &settings).unwrap();

        let read = load(None, Some(&project));
        assert!(read.completion.ghost_text);
        assert!(!read.completion.dropdown);
        assert_eq!(read.completion.debounce_ms, 300);

        // Committable, so it ends with a newline.
        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(raw.ends_with("}\n"), "{raw:?}");
        // And no temporary file survives the rename.
        let left: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["settings.json"], "{left:?}");
    }

    /// What the dialogue needs to offer a choice: where each layer is, and
    /// whether it is there yet.
    #[test]
    fn the_layers_say_where_they_are_and_whether_they_exist() {
        let (_dir, user, project) = fixture();
        write(&user, "{}");

        let found = layers(Some(&user), Some(&project));
        assert_eq!(found[0].scope, Scope::User);
        assert!(found[0].present, "written just now");
        assert!(found[0].shown.ends_with(USER_FILE), "{}", found[0].shown);

        assert_eq!(found[1].scope, Scope::Project);
        assert!(!found[1].present, "no file in the folder yet");
        assert_eq!(found[1].shown, PROJECT_FILE);

        // No folder open: the project layer has nowhere to be.
        let none = layers(Some(&user), None);
        assert!(none[1].file.is_none());
        assert!(!none[1].present);
    }

    /// A debounce of zero is a request per character typed, which is the one
    /// setting here that costs money to get wrong.
    #[test]
    fn a_debounce_that_would_ask_on_every_keystroke_is_refused() {
        let mut settings = Settings::default();
        settings.completion.ghost_text = true;
        settings.completion.debounce_ms = 0;
        assert!(settings.clone().checked().is_err());

        // Only when there is something to ask: with ghost text off the number
        // means nothing, and refusing it would be refusing a value nobody uses.
        settings.completion.ghost_text = false;
        assert!(settings.clone().checked().is_ok());

        settings.completion.ghost_text = true;
        settings.completion.debounce_ms = MIN_DEBOUNCE_MS;
        assert!(settings.checked().is_ok());
    }
}
