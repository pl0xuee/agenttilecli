//! A `CODEX_HOME` of our own, so codex's hooks can be installed without
//! touching the user's.
//!
//! Claude takes `--settings`, which layers a file over whatever the user has
//! and applies only to the process being launched. Codex has no equivalent:
//! hooks are read from its home directory or from the repo, and from nowhere
//! else. Writing to `~/.codex` would mean every codex the user runs, in every
//! terminal, for the rest of time, calling a binary this app installed - which
//! is exactly the thing the claude side goes to some trouble not to do.
//!
//! What codex does honour is `CODEX_HOME`. So this builds one: a cache
//! directory holding a symlink to every entry of the real home - auth, config,
//! sessions, history - and one real file of our own beside them. Codex reads
//! the user's actual settings through the links, and our hooks from the file,
//! and `~/.codex` is never opened for writing.
//!
//! Symlinks rather than copies because one of those entries is `auth.json`.
//! Copying somebody's credentials into a cache directory to save a file read is
//! a poor trade, and a copy would go stale the moment they re-authenticated.
//!
//! Rebuilt on every launch rather than only when absent, for the same reason
//! the claude settings file is: a hook left behind by an older build must not
//! outlive the version that wrote it.
//!
//! Its own module rather than more of `pane` because there is no GTK in it.
//! That is what lets the whole of it be tested against a temp directory instead
//! of a window - and this is the half of codex support most likely to be wrong,
//! since it is the half built from documentation rather than from a binary.

use std::path::{Path, PathBuf};

/// Our file. Everything else in the directory is a link to the user's.
const HOOKS_FILE: &str = "hooks.json";

/// Builds the private home at `into`, mirroring `real` and installing
/// `hooks_json`.
pub fn build(real: &Path, into: &Path, hooks_json: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(into)?;

    // Prune first, so a link whose target the user has since deleted is gone
    // before codex reads the directory. A home that used to have a file and
    // now doesn't should stop advertising one.
    for entry in std::fs::read_dir(into)?.flatten() {
        let path = entry.path();
        let is_link = std::fs::symlink_metadata(&path)
            .map(|m| m.is_symlink())
            .unwrap_or(false);
        if is_link && !path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }

    // `hooks.json` is ours and is handled below: mirroring theirs would shadow
    // ours, or ours theirs, and both of those lose somebody their hooks.
    if let Ok(entries) = std::fs::read_dir(real) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == std::ffi::OsStr::new(HOOKS_FILE) {
                continue;
            }
            let link = into.join(&name);
            if std::fs::symlink_metadata(&link).is_ok() {
                continue;
            }
            let _ = std::os::unix::fs::symlink(entry.path(), link);
        }
    }

    // Their hooks are something they set up on purpose, for reasons that have
    // nothing to do with this app. Ours are added to theirs rather than
    // replacing them - somebody who loses their own hooks by opening a window
    // manager has no reason at all to connect the two events.
    let merged = match std::fs::read_to_string(real.join(HOOKS_FILE)) {
        Ok(theirs) => merge(&theirs, hooks_json),
        Err(_) => hooks_json.to_string(),
    };
    std::fs::write(into.join(HOOKS_FILE), merged)
}

/// Adds our event entries to theirs, keeping both.
///
/// Anything unparseable on their side means ours alone: a codex that cannot
/// read their file was not going to run their hooks either, and losing our dots
/// as well would help nobody.
fn merge(theirs: &str, ours: &str) -> String {
    let (Ok(mut theirs), Ok(ours)) = (
        serde_json::from_str::<serde_json::Value>(theirs),
        serde_json::from_str::<serde_json::Value>(ours),
    ) else {
        return ours.to_string();
    };

    let Some(our_events) = ours["hooks"].as_object().cloned() else {
        return ours.to_string();
    };
    if !theirs["hooks"].is_object() {
        theirs["hooks"] = serde_json::json!({});
    }
    let their_events = theirs["hooks"]
        .as_object_mut()
        .expect("just replaced with an object if it wasn't one");
    for (event, entries) in our_events {
        let slot = their_events
            .entry(event)
            .or_insert_with(|| serde_json::json!([]));
        match (slot.as_array_mut(), entries.as_array()) {
            (Some(slot), Some(entries)) => slot.extend(entries.iter().cloned()),
            // Their value for this event isn't a list at all. It is not this
            // app's job to have an opinion about that, but it is this app's job
            // to still get its own hook registered.
            _ => *slot = entries,
        }
    }
    theirs.to_string()
}

/// The private home for this run, or `None` if it could not be built - in which
/// case the pane launches a plain `codex`, exactly as a claude pane launches a
/// plain `claude` when its settings file cannot be written. A silent agent is a
/// great deal better than no agent.
pub fn prepare(hook_bin: &str, bell_hook: &str) -> Option<PathBuf> {
    let into = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?
        .join("agenttilecli")
        .join("codex-home");

    // Their `CODEX_HOME` if they set one, since somebody who has moved it has
    // done so on purpose and their auth lives at the new address.
    let real = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))?;

    build(
        &real,
        &into,
        &crate::hooks::codex_hooks_json(hook_bin, bell_hook),
    )
    .ok()?;
    Some(into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A scratch pair of directories, unique per test so the suite can run
    /// them in parallel the way it runs everything else.
    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("atc-codex-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let real = base.join("real");
        let into = base.join("into");
        fs::create_dir_all(&real).expect("scratch real");
        (real, into)
    }

    #[test]
    fn the_users_own_home_is_mirrored_and_never_written_to() {
        let (real, into) = scratch("mirror");
        fs::write(real.join("auth.json"), "{\"token\":\"secret\"}").unwrap();
        fs::create_dir(real.join("sessions")).unwrap();

        build(&real, &into, "{\"hooks\":{}}").expect("builds");

        assert_eq!(
            fs::read_to_string(into.join("auth.json")).unwrap(),
            "{\"token\":\"secret\"}",
            "the pane's codex has to still be logged in",
        );
        assert!(
            fs::symlink_metadata(into.join("auth.json"))
                .unwrap()
                .is_symlink(),
            "mirrored as a link, so their credentials are never copied about",
        );
        assert!(into.join("sessions").exists(), "directories mirror too");
        assert!(
            !real.join("hooks.json").exists(),
            "and the user's own home gained nothing at all",
        );
    }

    #[test]
    fn our_hooks_land_as_a_real_file() {
        let (real, into) = scratch("hooks");
        build(&real, &into, "{\"hooks\":{\"Stop\":[]}}").expect("builds");

        let written = into.join("hooks.json");
        assert!(!fs::symlink_metadata(&written).unwrap().is_symlink());
        assert_eq!(
            fs::read_to_string(&written).unwrap(),
            "{\"hooks\":{\"Stop\":[]}}",
        );
    }

    /// Somebody who already uses codex hooks must not lose them by opening this
    /// app - they would have no reason on earth to connect the two.
    #[test]
    fn an_existing_hooks_file_is_merged_rather_than_shadowed() {
        let (real, into) = scratch("merge");
        fs::write(
            real.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"theirs"}]}]}}"#,
        )
        .unwrap();

        build(
            &real,
            &into,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"ours"}]}],"Stop":[{"hooks":[{"type":"command","command":"ours"}]}]}}"#,
        )
        .expect("builds");

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(into.join("hooks.json")).unwrap()).unwrap();

        let commands: Vec<String> = merged["hooks"]["PreToolUse"]
            .as_array()
            .expect("an array")
            .iter()
            .flat_map(|e| e["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|h| h["command"].as_str().map(str::to_string))
            .collect();
        assert!(
            commands.iter().any(|c| c == "theirs"),
            "their hook survived: {commands:?}",
        );
        assert!(
            commands.iter().any(|c| c == "ours"),
            "and ours was added: {commands:?}",
        );
        assert!(
            !merged["hooks"]["Stop"].is_null(),
            "including for events they had none for",
        );
    }

    /// Codex may never have been run on this machine. Finding that out is not
    /// this app's job, so it builds the home anyway and lets codex do whatever
    /// it does on a first run.
    #[test]
    fn a_home_that_does_not_exist_yet_is_not_an_error() {
        let (real, into) = scratch("absent");
        fs::remove_dir_all(&real).unwrap();

        build(&real, &into, "{\"hooks\":{}}").expect("an absent home is fine");
        assert!(into.join("hooks.json").exists());
    }

    /// A link to something the user has since deleted is a broken entry in a
    /// directory codex is about to read, and it costs nothing to drop.
    #[test]
    fn links_to_things_that_have_gone_are_pruned() {
        let (real, into) = scratch("prune");
        fs::write(real.join("stale.json"), "x").unwrap();
        build(&real, &into, "{}").expect("builds");
        assert!(into.join("stale.json").exists());

        fs::remove_file(real.join("stale.json")).unwrap();
        build(&real, &into, "{}").expect("rebuilds");
        assert!(
            fs::symlink_metadata(into.join("stale.json")).is_err(),
            "a link to a file that has gone was left behind",
        );
    }

    /// Their file being unreadable is not a reason for this app's own dots to
    /// stop working.
    #[test]
    fn hooks_we_cannot_read_leave_ours_installed_anyway() {
        let (real, into) = scratch("garbled");
        fs::write(real.join("hooks.json"), "{ not json at all").unwrap();

        build(&real, &into, r#"{"hooks":{"Stop":[]}}"#).expect("builds");

        let ours: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(into.join("hooks.json")).unwrap())
                .expect("what we wrote is readable even if what we read wasn't");
        assert!(!ours["hooks"]["Stop"].is_null());
    }

    /// The rebuild-every-launch rule, which is what stops a hook written by an
    /// older build outliving the version that wrote it.
    #[test]
    fn a_second_build_replaces_the_hooks_rather_than_appending_to_them() {
        let (real, into) = scratch("rebuild");
        build(&real, &into, r#"{"hooks":{"Stop":[{"hooks":[]}]}}"#).expect("builds");
        build(&real, &into, r#"{"hooks":{"Stop":[{"hooks":[]}]}}"#).expect("rebuilds");

        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(into.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(
            written["hooks"]["Stop"].as_array().expect("an array").len(),
            1,
            "our own hooks accumulated across launches",
        );
    }
}
