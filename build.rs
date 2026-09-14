//! Records the crate's git identity for the `runtime_info` control-plane verb.
//! A missing git checkout yields no commit; the runtime reports that truthfully.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for path in [
        ".git/HEAD",
        ".git/index",
        "../.git/modules/ddlog-runtime/HEAD",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let commit = git(&["rev-parse", "HEAD"]).filter(|hash| hash.len() == 40);
    let dirty = commit.is_some()
        && git(&["status", "--porcelain", "--untracked-files=no"])
            .is_some_and(|status| !status.is_empty());
    println!(
        "cargo:rustc-env=DDLOG_RUNTIME_COMMIT={}",
        commit.unwrap_or_default()
    );
    println!("cargo:rustc-env=DDLOG_RUNTIME_DIRTY={dirty}");
}
