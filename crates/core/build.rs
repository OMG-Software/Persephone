//! Records the commit the core is built from as `PERSEPHONE_REVISION`, which
//! `app_status` reports: the first 12 hex digits of the commit ID, or
//! `unknown`. It names the checked-out commit only; uncommitted changes are
//! not shown. Without a usable Git checkout of this source tree (a source
//! archive, perhaps inside another repository, or a container where Git
//! refuses the directory) it falls back to `GITHUB_SHA`.

use std::{path::Path, process::Command};

const LENGTH: usize = 12;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("set by Cargo");
    let own = own_checkout(&dir);
    let revision = own
        .then(|| git(&dir, &["rev-parse", "HEAD"]))
        .flatten()
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .and_then(|id| short(&id))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=PERSEPHONE_REVISION={revision}");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    if !own {
        return;
    }
    // Run again when the checked-out commit changes: HEAD itself (another
    // branch), the branch it names (a new commit) and packed refs.
    let mut watched = vec!["HEAD".to_string(), "packed-refs".to_string()];
    if let Some(branch) = git(&dir, &["symbolic-ref", "-q", "HEAD"]) {
        watched.push(branch);
    }
    for name in watched {
        if let Some(path) = git(&dir, &["rev-parse", "--git-path", &name]) {
            let path = Path::new(&dir).join(path);
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// Whether Git's worktree for `dir` is this source tree, whose root is two
/// levels above this crate. Git looks upwards for a repository, so a source
/// archive unpacked inside another repository would otherwise report that
/// repository's commit.
fn own_checkout(dir: &str) -> bool {
    let root = Path::new(dir).join("..").join("..");
    let top = git(dir, &["rev-parse", "--show-toplevel"]);
    match (top.map(std::fs::canonicalize), std::fs::canonicalize(root)) {
        (Some(Ok(top)), Ok(root)) => top == root,
        _ => false,
    }
}

/// One line of Git's output, or `None` when Git is missing or fails.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let line = text.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

/// The first [`LENGTH`] digits of a full commit ID; anything else is refused.
fn short(id: &str) -> Option<String> {
    let id = id.trim();
    let hex = id.len() >= 40 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex.then(|| id[..LENGTH].to_owned())
}
