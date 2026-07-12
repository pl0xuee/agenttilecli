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

fn main() {
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    println!("cargo:rustc-env=AGENTTILECLI_GIT_BRANCH={branch}");

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
