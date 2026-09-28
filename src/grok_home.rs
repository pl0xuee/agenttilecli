//! A private `GROK_HOME`, used to add pane-local status hooks without changing
//! the user's Grok installation.
//!
//! Grok Build discovers global hook files only below `$GROK_HOME/hooks/`.
//! Pointing a pane at the user's real home and writing there would make every
//! Grok process on the machine call back into AgentTileCLI. Instead, this
//! module mirrors the real home with symlinks, creates one real `hooks/`
//! directory in the app's cache, mirrors the user's hook files inside it, and
//! writes our own hook beside them. Auth, config, sessions, plugins and other
//! state therefore remain the user's; only hook discovery is overlaid.

use std::path::{Path, PathBuf};

const HOOKS_DIR: &str = "hooks";
const HOOKS_FILE: &str = "agenttilecli.json";

/// Builds the mirrored home at `into` and installs `hooks_json` without
/// writing anything below `real`.
pub fn build(real: &Path, into: &Path, hooks_json: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(into)?;

    prune_broken_links(into)?;
    if let Ok(entries) = std::fs::read_dir(real) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == std::ffi::OsStr::new(HOOKS_DIR) {
                continue;
            }
            link_if_absent(&entry.path(), &into.join(name));
        }
    }

    // This path must be a directory owned by the private home. In particular,
    // never follow an old `hooks` symlink: writing our file through one would
    // violate the promise above and modify the user's real home.
    let private_hooks = into.join(HOOKS_DIR);
    match std::fs::symlink_metadata(&private_hooks) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            std::fs::remove_file(&private_hooks)?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::create_dir_all(&private_hooks)?;

    prune_broken_links(&private_hooks)?;
    let real_hooks = real.join(HOOKS_DIR);
    if let Ok(entries) = std::fs::read_dir(&real_hooks) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == std::ffi::OsStr::new(HOOKS_FILE) {
                continue;
            }
            link_if_absent(&entry.path(), &private_hooks.join(name));
        }
    }

    // The filename is ours, but preserve a same-named user hook if one exists;
    // a harmless name collision must not make somebody's automation vanish.
    let merged = match std::fs::read_to_string(real_hooks.join(HOOKS_FILE)) {
        Ok(theirs) => crate::hooks::merge_hook_files(&theirs, hooks_json),
        Err(_) => hooks_json.to_string(),
    };
    crate::hooks::write_if_changed(&private_hooks.join(HOOKS_FILE), &merged)
}

fn link_if_absent(source: &Path, link: &Path) {
    crate::codex_home::mirror(source, link);
}

fn prune_broken_links(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let broken_link = std::fs::symlink_metadata(&path)
            .map(|metadata| metadata.file_type().is_symlink() && !path.exists())
            .unwrap_or(false);
        if broken_link {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

/// Prepares the private home used by a Grok pane. Failure is deliberately
/// silent: the pane can still launch a normal Grok session without status dots.
pub fn prepare(hook_bin: &str, bell_hook: &str) -> Option<PathBuf> {
    let into = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?
        .join("agenttilecli")
        .join("grok-home");
    let real = std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".grok")))?;

    // A GUI normally cannot inherit this value, but following it would turn
    // the private overlay into the user's configured home and write there.
    if crate::codex_home::same_directory(&real, &into) {
        return None;
    }
    build(
        &real,
        &into,
        &crate::hooks::grok_hooks_json(hook_bin, bell_hook),
    )
    .ok()?;
    Some(into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("atc-grok-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let real = base.join("real");
        let into = base.join("into");
        fs::create_dir_all(&real).expect("scratch real");
        (real, into)
    }

    #[test]
    fn user_state_and_hooks_are_mirrored_without_writing_to_their_home() {
        let (real, into) = scratch("mirror");
        fs::write(real.join("auth.json"), "{\"token\":\"secret\"}").unwrap();
        fs::create_dir(real.join(HOOKS_DIR)).unwrap();
        fs::write(real.join(HOOKS_DIR).join("theirs.json"), "{\"hooks\":{}}").unwrap();

        build(&real, &into, "{\"hooks\":{\"Stop\":[]}}").expect("builds");

        assert!(
            fs::symlink_metadata(into.join("auth.json"))
                .unwrap()
                .is_symlink()
        );
        assert!(
            fs::symlink_metadata(into.join(HOOKS_DIR).join("theirs.json"))
                .unwrap()
                .is_symlink(),
            "their hooks stay active through links",
        );
        assert!(into.join(HOOKS_DIR).join(HOOKS_FILE).is_file());
        assert!(!real.join(HOOKS_DIR).join(HOOKS_FILE).exists());
    }

    #[test]
    fn the_private_home_carries_the_native_grok_hook_payload() {
        let (real, into) = scratch("native");
        let payload = crate::hooks::grok_hooks_json("/usr/bin/agenttilecli", "true");

        build(&real, &into, &payload).expect("builds");

        let written: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(into.join(HOOKS_DIR).join(HOOKS_FILE)).unwrap(),
        )
        .unwrap();
        for event in crate::hooks::Event::ALL {
            assert!(
                written["hooks"][event.name()].is_array(),
                "missing {event:?}"
            );
        }
    }

    #[test]
    fn a_same_named_user_hook_is_merged_with_ours() {
        let (real, into) = scratch("merge");
        fs::create_dir(real.join(HOOKS_DIR)).unwrap();
        fs::write(
            real.join(HOOKS_DIR).join(HOOKS_FILE),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"theirs"}]}]}}"#,
        )
        .unwrap();

        build(
            &real,
            &into,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"ours"}]}]}}"#,
        )
        .expect("builds");

        let merged: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(into.join(HOOKS_DIR).join(HOOKS_FILE)).unwrap(),
        )
        .unwrap();
        let commands: Vec<_> = merged["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|hook| hook["command"].as_str().map(str::to_string))
            .collect();
        assert!(commands.iter().any(|command| command == "theirs"));
        assert!(commands.iter().any(|command| command == "ours"));
    }

    #[test]
    fn an_old_hooks_symlink_is_never_written_through() {
        let (real, into) = scratch("symlink");
        let real_hooks = real.join(HOOKS_DIR);
        fs::create_dir(&real_hooks).unwrap();
        fs::create_dir_all(&into).unwrap();
        std::os::unix::fs::symlink(&real_hooks, into.join(HOOKS_DIR)).unwrap();

        build(&real, &into, "{\"hooks\":{}}").expect("builds");

        assert!(
            !fs::symlink_metadata(into.join(HOOKS_DIR))
                .unwrap()
                .is_symlink()
        );
        assert!(!real_hooks.join(HOOKS_FILE).exists());
        assert!(into.join(HOOKS_DIR).join(HOOKS_FILE).exists());
    }
}
