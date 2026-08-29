use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo always supplies CARGO_MANIFEST_DIR"),
    );
    let (git_sha, watched_paths) = git_build_identity(&manifest_dir);
    for path in watched_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rustc-env=TEA_BUILD_GIT_SHA={git_sha}");
}

fn git_build_identity(manifest_dir: &Path) -> (String, Vec<PathBuf>) {
    let mut watched_paths = Vec::new();
    let git_dir = run_git(manifest_dir, &["rev-parse", "--git-dir"]).and_then(|output| {
        let path = PathBuf::from(output.trim());
        let path = if path.is_absolute() {
            path
        } else {
            manifest_dir.join(path)
        };
        path.is_dir().then_some(path)
    });
    if let Some(git_dir) = git_dir {
        let head = git_dir.join("HEAD");
        watched_paths.push(head.clone());
        for relative in ["packed-refs", "logs/HEAD"] {
            let path = git_dir.join(relative);
            if path.exists() {
                watched_paths.push(path);
            }
        }
        if let Ok(contents) = fs::read_to_string(&head) {
            if let Some(reference) = contents.trim().strip_prefix("ref: ") {
                watched_paths.push(git_dir.join(reference));
            }
        }
    }
    let sha = run_git(manifest_dir, &["rev-parse", "--short=7", "HEAD"])
        .map(|output| output.trim().to_owned())
        .filter(|sha| sha.len() == 7 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".into());
    (sha, watched_paths)
}

fn run_git(manifest_dir: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
