use std::process::Command;

/// The checkout this binary was built from - baked in at compile time.
/// `install.sh` builds straight out of the user's clone and copies the
/// binary to ~/.local/bin, so this is where the sources it was built from
/// still live, and the only place an update can be pulled and rebuilt.
/// Users who move or delete their clone get a `Status::Failed` saying so
/// rather than a silent no-op.
const REPO_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// The commit `REPO_DIR` was sitting on when this binary was built - which
/// is what "am I up to date?" actually compares against. Deliberately not
/// the checkout's *current* HEAD: someone can pull without rebuilding, and
/// the running binary is still the old one until `install.sh` runs again.
/// Empty when built outside a git checkout (a source tarball, say), in
/// which case the checkout's HEAD is the best guess available.
const BUILT_COMMIT: &str = env!("AGENTTILECLI_GIT_COMMIT");

/// The branch releases land on, and so the only thing this checks against -
/// `master` on the `origin` remote.
const UPSTREAM: &str = "origin/master";

/// How many of the new commits' subjects to show in the update dialog before
/// summarizing the rest as a count.
const MAX_SUBJECTS: usize = 6;

pub enum Status {
    /// The built commit already contains everything on `origin/master`.
    UpToDate,
    Available(Update),
    /// The check itself couldn't be completed (no git, no network, the
    /// checkout is gone, ...). The string is a human-readable reason.
    Failed(String),
}

pub struct Update {
    /// How many commits `origin/master` is ahead of the built commit.
    pub commits: usize,
    /// Subject lines of those commits, newest first, capped at `MAX_SUBJECTS`.
    pub subjects: Vec<String>,
    /// `None` when the update can be applied in place (see `command`);
    /// otherwise why it can't, phrased for a dialog.
    pub blocked: Option<String>,
}

/// Runs a git command in the checkout, returning its trimmed stdout.
///
/// `GIT_TERMINAL_PROMPT=0` matters: a `fetch` that decides it wants
/// credentials would otherwise sit forever waiting on a prompt that a GUI
/// app has no terminal to show - a hang rather than an error. The repo is
/// public, so no auth is expected in the first place; this just makes the
/// unexpected case fail fast and visibly.
fn git(repo: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("couldn't run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("`git {}` failed", args.join(" "))
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether the git command succeeds - for the `--is-ancestor` style checks,
/// where a non-zero exit is a legitimate "no", not an error.
fn git_ok(repo: &str, args: &[&str]) -> bool {
    git(repo, args).is_ok()
}

/// Fetches `origin/master` and compares it against the commit this binary
/// was built from.
///
/// Blocking (it hits the network), so callers must run it off the main loop
/// - see `Groups::check_for_updates`, which hands it to `gio::spawn_blocking`.
pub fn check() -> Status {
    check_repo(REPO_DIR, BUILT_COMMIT)
}

/// The real work behind `check`, with the checkout and the built commit
/// passed in rather than baked in - so the tests can drive it against
/// throwaway repos in every state that matters (behind, diverged, dirty, on
/// the wrong branch) instead of only ever seeing whichever state the
/// developer's own checkout happens to be in.
fn check_repo(repo: &str, built_commit: &str) -> Status {
    if let Err(e) = git(repo, &["rev-parse", "--git-dir"]) {
        return Status::Failed(format!(
            "The source checkout this build came from is no longer a git \
             repository:\n\n{repo}\n\n{e}\n\nRe-clone it and run ./install.sh to update."
        ));
    }

    // Without the built commit there is nothing to compare against, and the
    // tempting fallback - the checkout's current HEAD - would quietly answer
    // the wrong question: it reports "up to date" for a checkout that has been
    // pulled but not rebuilt, which is the single case this feature exists to
    // catch. Refusing to guess is the only honest option.
    //
    // build.rs leaves this empty when it can't run `git rev-parse HEAD`: a
    // build under `sudo`, or in a container where git distrusts the checkout's
    // ownership (`safe.directory`), are the usual causes.
    if built_commit.is_empty() {
        return Status::Failed(format!(
            "This build doesn't know which commit it was built from, so it \
             can't tell whether it's out of date.\n\nRebuild it as your own \
             user (not under sudo) with ./install.sh in:\n\n{repo}"
        ));
    }

    // Just the one branch, and no tags/prune: this only ever reads
    // origin/master, and a check for updates shouldn't reshape refs the
    // user's own work depends on.
    //
    // The low-speed limits are the other half of `GIT_TERMINAL_PROMPT=0`: that
    // stops a credential prompt hanging forever, and these stop a black-holed
    // connection (a captive portal, a dropped VPN) doing the same. Without
    // them the button sits on "Checking..." indefinitely, with no way to
    // cancel it. Under 1KB/s for 20s straight and git gives up.
    let fetch = git(
        repo,
        &[
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=20",
            "fetch",
            "--quiet",
            "origin",
            "master",
        ],
    );
    if let Err(e) = fetch {
        // Deliberately not "couldn't reach GitHub": this same failure covers a
        // checkout with no `origin` remote, and an `origin` that has no
        // `master` branch at all - neither of which is a network problem, and
        // claiming otherwise would send the user hunting for the wrong fault.
        return Status::Failed(format!(
            "Couldn't fetch `{UPSTREAM}` to check for updates.\n\n{e}"
        ));
    }

    // A commit baked in by a build that has since been rebased/gc'd away is
    // no longer comparable - fall back to the checkout's HEAD, which at
    // worst reports "up to date" for a stale binary the user can rebuild
    // anyway.
    let baseline = if !built_commit.is_empty() && git_ok(repo, &["cat-file", "-e", built_commit]) {
        built_commit
    } else {
        "HEAD"
    };

    if git_ok(repo, &["merge-base", "--is-ancestor", UPSTREAM, baseline]) {
        return Status::UpToDate;
    }

    let range = format!("{baseline}..{UPSTREAM}");
    let commits = match git(repo, &["rev-list", "--count", &range]) {
        // An unparseable count is reported rather than swallowed. Treating it
        // as zero - the obvious shortcut - would fall through to `UpToDate`
        // below, and "you're up to date" is the one answer this must never
        // give by accident.
        Ok(count) => match count.parse::<usize>() {
            Ok(commits) => commits,
            Err(_) => {
                return Status::Failed(format!(
                    "Couldn't count the new commits - git answered `{count}`."
                ))
            }
        },
        Err(e) => return Status::Failed(e),
    };
    if commits == 0 {
        // Not an ancestor, yet nothing to pull: the checkout has diverged
        // from master rather than fallen behind it. Nothing to offer.
        return Status::UpToDate;
    }

    let subjects = git(
        repo,
        &[
            "log",
            "--format=%s",
            &format!("--max-count={MAX_SUBJECTS}"),
            &range,
        ],
    )
    .map(|out| out.lines().map(str::to_string).collect())
    .unwrap_or_default();

    Status::Available(Update {
        commits,
        subjects,
        blocked: blocked_reason(repo),
    })
}

/// Why the update can't be pulled and rebuilt in place, if it can't.
///
/// The bar is deliberately high: this app's update button rebuilds the
/// user's own checkout, so it only ever touches one that is unambiguously a
/// stock, unmodified copy of master. Anything else (a dev branch, local
/// commits, edited files) is someone's work in progress, and quietly
/// fast-forwarding over it would be a far worse outcome than telling them
/// to update by hand.
fn blocked_reason(repo: &str) -> Option<String> {
    let Ok(branch) = git(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]) else {
        return Some("its HEAD is detached".to_string());
    };
    if branch != "master" {
        return Some(format!("it's on branch `{branch}`, not `master`"));
    }
    // `--untracked-files=no` matters: a bare `--porcelain` also lists untracked
    // files, so a stray editor swapfile or a `notes.txt` sitting in the clone
    // would block the update and tell the user to deal with "uncommitted
    // changes" they haven't made. A fast-forward doesn't touch untracked files
    // anyway - only tracked ones can be overwritten, and those are what this
    // is guarding.
    if !git(repo, &["status", "--porcelain", "--untracked-files=no"]).is_ok_and(|s| s.is_empty()) {
        return Some("it has uncommitted changes".to_string());
    }
    if !git_ok(repo, &["merge-base", "--is-ancestor", "HEAD", UPSTREAM]) {
        return Some("it has local commits that aren't on `origin/master`".to_string());
    }
    None
}

/// Quotes a path for POSIX `sh`, so a checkout under a directory with spaces
/// (or quotes) still works. Also used by `pane` to quote claude's settings
/// path, which has the same problem.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The shell command that performs the update, to be run in a pane so the
/// user watches the pull and the `cargo build` happen rather than staring at
/// a frozen dialog.
///
/// Written to a script file and invoked as `sh <file>` rather than passed as
/// shell source directly, because the pane runs it through the user's *login*
/// shell (for PATH - same reason panes run `$SHELL -lc claude`), and that
/// shell may well be fish, whose syntax shares almost nothing with this. The
/// login shell only ever sees `exec sh <path>`, which is valid in all of them.
///
/// The trailing `read` is what keeps the pane open after the script finishes:
/// a pane closes itself the moment its child exits (`connect_child_exited`),
/// which would otherwise make the result - success or failure - vanish
/// instantly.
pub fn command() -> Result<String, String> {
    // The `cd` is inside the `if`, not a `cd ... || exit` ahead of it, so that
    // a checkout that has gone missing between the check and the click still
    // lands in the failure branch and still reaches the `read` below. Exiting
    // early instead would close the pane the instant it opened, taking the
    // reason why with it.
    let script = format!(
        "printf '\\033[1;36m== Updating AgentTileCLI ==\\033[0m\\n\\n'\n\
         if cd {repo} && git pull --ff-only origin master && ./install.sh; then\n\
         \tprintf '\\n\\033[1;32mUpdate complete.\\033[0m Quit and relaunch AgentTileCLI to run the new version.\\n'\n\
         else\n\
         \tprintf '\\n\\033[1;31mUpdate failed.\\033[0m See the output above; the installed version is unchanged.\\n'\n\
         fi\n\
         printf '\\nPress Enter to close this pane... '\n\
         read -r _\n",
        repo = sh_quote(REPO_DIR),
    );

    // The script goes in the checkout's own `target/`, under a fixed name, and
    // pointedly *not* in a shared /tmp.
    //
    // A fixed path in /tmp is the obvious choice and a dangerous one:
    // `fs::write` follows symlinks, so on a multi-user box anyone else could
    // pre-create `/tmp/agenttilecli-update.sh` as a symlink into this user's
    // $HOME and have us truncate whatever it points at. `target/` is inside
    // the user's own checkout - the very directory this script is about to
    // rebuild - so no other user can win that race. It's gitignored, too, so
    // it can't dirty the checkout that `blocked_reason` inspects.
    let dir = std::path::Path::new(REPO_DIR).join("target");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let path = dir.join("agenttilecli-update.sh");
    std::fs::write(&path, script).map_err(|e| format!("couldn't write the update script: {e}"))?;

    Ok(format!("exec sh {}", sh_quote(&path.to_string_lossy())))
}

/// The directory the update pane should run in - the checkout being updated,
/// so the pane's corner label names it.
pub fn repo_dir() -> &'static str {
    REPO_DIR
}

/// The running build, for the "you're up to date" dialog: `0.2.0 (0603294)`,
/// or just the version when the commit isn't known.
pub fn version() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match BUILT_COMMIT.get(..7) {
        Some(short) => format!("{version} ({short})"),
        None => version.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Runs a git command in `repo`, panicking on failure - the test setup's
    /// own git calls, where anything but success means the fixture is broken
    /// and the assertions below would be meaningless anyway.
    fn run(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            // Committing needs an identity, and the machine running the
            // tests may not have (or want to use) a global one. Signing is
            // off for the same reason: a developer with commit.gpgsign=true
            // globally shouldn't get a passphrase prompt out of `cargo test`.
            .args(["-c", "user.email=test@example.com"])
            .args(["-c", "user.name=Test"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit(repo: &Path, message: &str) -> String {
        let counter = {
            static N: AtomicUsize = AtomicUsize::new(0);
            N.fetch_add(1, Ordering::Relaxed)
        };
        std::fs::write(repo.join("file.txt"), format!("{message} {counter}")).expect("write");
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", message]);
        run(repo, &["rev-parse", "HEAD"])
    }

    /// A throwaway "user's checkout" wired to a throwaway "origin", both on
    /// disk, so `check_repo`'s `git fetch origin master` is a real fetch that
    /// never touches the network. Returns the checkout and the commit its
    /// (pretend) installed binary was built from.
    ///
    /// The checkout starts one commit behind origin/master, clean, on master:
    /// exactly the state a user who installed a release and then fell behind
    /// is in. Each test mutates it from there.
    fn fixture(name: &str) -> (PathBuf, String) {
        let root =
            std::env::temp_dir().join(format!("agenttilecli-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let origin = root.join("origin.git");
        let work = root.join("work");
        std::fs::create_dir_all(&origin).expect("mkdir origin");
        std::fs::create_dir_all(&work).expect("mkdir work");

        run(
            &origin,
            &["init", "-q", "--bare", "--initial-branch=master"],
        );
        run(&work, &["init", "-q", "--initial-branch=master"]);
        run(
            &work,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );

        let built = commit(&work, "the release the user installed");
        run(&work, &["push", "-q", "origin", "master"]);

        // The newer commit lands on origin/master from a *second* clone, never
        // from `work` itself. Pushing from `work` would be simpler, but it also
        // advances work's own refs/remotes/origin/master as a side effect - so
        // every test below would still pass even if `check_repo`'s fetch never
        // moved the tracking ref at all. That fetch is the one thing the whole
        // feature rests on, and this is what leaves it actually under test:
        // work's origin/master starts out stale, and only the fetch can fix it.
        let other = root.join("other");
        run(
            &root,
            &[
                "clone",
                "-q",
                &origin.to_string_lossy(),
                &other.to_string_lossy(),
            ],
        );
        commit(&other, "a shiny new feature");
        run(&other, &["push", "-q", "origin", "master"]);

        (work, built)
    }

    #[test]
    fn clean_master_behind_origin_is_an_offerable_update() {
        let (work, built) = fixture("behind");
        let Status::Available(update) = check_repo(&work.to_string_lossy(), &built) else {
            panic!("expected an available update");
        };
        assert_eq!(update.commits, 1);
        assert_eq!(update.subjects, ["a shiny new feature"]);
        assert_eq!(update.blocked, None, "a clean master checkout is updatable");
    }

    #[test]
    fn a_build_already_containing_origin_master_is_up_to_date() {
        let (work, _built) = fixture("current");
        // The binary was built from origin/master itself - the case for
        // anyone who just ran ./install.sh.
        run(&work, &["fetch", "-q", "origin", "master"]);
        let tip = run(&work, &["rev-parse", "origin/master"]);
        assert!(matches!(
            check_repo(&work.to_string_lossy(), &tip),
            Status::UpToDate,
        ));
    }

    /// The safety net: an update is still *reported*, but never applied over
    /// someone's uncommitted work or their own branch. Getting this wrong
    /// means fast-forwarding away a developer's changes.
    #[test]
    fn a_dirty_or_diverged_checkout_is_reported_but_not_updatable() {
        let (work, built) = fixture("dirty");
        let repo = work.to_string_lossy().to_string();

        std::fs::write(work.join("file.txt"), "work in progress").expect("write");
        let Status::Available(update) = check_repo(&repo, &built) else {
            panic!("expected an available update");
        };
        assert_eq!(
            update.blocked.as_deref(),
            Some("it has uncommitted changes")
        );

        run(&work, &["checkout", "-q", "--", "file.txt"]);
        run(&work, &["switch", "-q", "-c", "dev"]);
        let Status::Available(update) = check_repo(&repo, &built) else {
            panic!("expected an available update");
        };
        assert_eq!(
            update.blocked.as_deref(),
            Some("it's on branch `dev`, not `master`"),
        );

        // Back on master, but with a local commit of their own on top: a
        // fast-forward would silently drop it.
        run(&work, &["switch", "-q", "master"]);
        commit(&work, "my own local commit");
        let Status::Available(update) = check_repo(&repo, &built) else {
            panic!("expected an available update");
        };
        assert_eq!(
            update.blocked.as_deref(),
            Some("it has local commits that aren't on `origin/master`"),
        );

        let _ = std::fs::remove_dir_all(work.parent().expect("root"));
    }

    /// A build that doesn't know its own commit must say so rather than fall
    /// back to the checkout's live HEAD. That fallback looks harmless and
    /// answers "up to date" for precisely the case this feature exists to
    /// catch: a checkout that has been pulled but never rebuilt, whose
    /// installed binary is still the old one.
    #[test]
    fn a_build_that_doesnt_know_its_commit_refuses_to_guess() {
        let (work, _built) = fixture("nocommit");
        let Status::Failed(reason) = check_repo(&work.to_string_lossy(), "") else {
            panic!("expected the check to refuse rather than guess");
        };
        assert!(
            reason.contains("doesn't know which commit"),
            "unexpected reason: {reason}",
        );
    }

    /// Untracked files survive a fast-forward untouched, so they're no reason
    /// to refuse one - and reporting them as "uncommitted changes" would send
    /// the user looking for edits they never made.
    #[test]
    fn an_untracked_file_does_not_block_the_update() {
        let (work, built) = fixture("untracked");
        std::fs::write(work.join("notes.txt"), "a scratch file").expect("write");

        let Status::Available(update) = check_repo(&work.to_string_lossy(), &built) else {
            panic!("expected an available update");
        };
        assert_eq!(
            update.blocked, None,
            "an untracked file is not a reason to refuse a fast-forward",
        );
    }

    #[test]
    fn a_checkout_that_is_gone_fails_the_check_rather_than_hanging() {
        let missing = std::env::temp_dir().join("agenttilecli-does-not-exist");
        let Status::Failed(reason) = check_repo(&missing.to_string_lossy(), "") else {
            panic!("expected the check to fail");
        };
        assert!(reason.contains("no longer a git repository"));
    }

    #[test]
    fn sh_quote_survives_quotes_and_spaces() {
        assert_eq!(sh_quote("/home/a b/repo"), "'/home/a b/repo'");
        assert_eq!(sh_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }

    /// The generated script must be POSIX `sh`, and must hold the pane open
    /// afterward - a pane whose child exits is removed, taking the output
    /// with it.
    #[test]
    fn update_script_is_shell_checkable_and_waits_before_exiting() {
        let Ok(command) = command() else {
            eprintln!("skipping: couldn't write the update script");
            return;
        };
        let path = command.strip_prefix("exec sh ").expect("runs via sh");
        let path = path.trim_matches('\'');

        let script = std::fs::read_to_string(path).expect("script written");
        assert!(script.contains("git pull --ff-only origin master"));
        assert!(script.contains("./install.sh"));
        assert!(script.trim_end().ends_with("read -r _"));

        // `sh -n` parses without executing: catches a syntax error in the
        // script above without anyone's checkout being pulled or rebuilt.
        let checked = Command::new("sh").arg("-n").arg(path).status();
        if let Ok(status) = checked {
            assert!(status.success(), "generated script is not valid POSIX sh");
        }
    }
}
