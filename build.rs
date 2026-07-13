use std::process::Command;

/// Resolves a path like "HEAD" or "refs/heads/master" to its real location
/// on disk. Needed because in a git worktree, `.git` is a redirect file, not
/// the actual git dir - `.git/HEAD` doesn't exist, so watching it literally
/// never detects branch switches or merges.
fn git_path(path: &str) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--git-path", path])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

/// The trimmed stdout of a git command, or `None` if git isn't there / the
/// command failed / this isn't a checkout at all (a source tarball, say).
fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

fn main() {
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();

    println!("cargo:rustc-env=AGENTTILECLI_GIT_BRANCH={branch}");

    // The commit this binary is built from, which `update::check` compares
    // against origin/master - not the checkout's HEAD at *run* time, since a
    // pull without a rebuild leaves the installed binary on the old commit.
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=AGENTTILECLI_GIT_COMMIT={commit}");
    if commit.is_empty() {
        // Not fatal - the app builds and runs fine without it - but the update
        // check refuses to guess a baseline it doesn't have, so say so at build
        // time rather than let it turn up as a puzzling dialog later. Building
        // under `sudo`, or in a container where git distrusts the checkout's
        // ownership, is the usual cause.
        println!(
            "cargo:warning=couldn't read the git commit being built; \
             checking for updates will be disabled in this binary"
        );
    }

    // Rebuild when switching branches...
    if let Some(head) = git_path("HEAD") {
        println!("cargo:rerun-if-changed={head}");
    }
    // ...and when merging into the branch that's already checked out, which
    // updates the branch's ref rather than HEAD itself.
    if !branch.is_empty() {
        if let Some(branch_ref) = git_path(&format!("refs/heads/{branch}")) {
            println!("cargo:rerun-if-changed={branch_ref}");
        }
    }
    if let Some(packed) = git_path("packed-refs") {
        println!("cargo:rerun-if-changed={packed}");
    }
}
