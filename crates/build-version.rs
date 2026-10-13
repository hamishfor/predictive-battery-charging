//! Shared build script for pbcd and pbcctl. Sets PBC_VERSION: the crate
//! version, plus the git commit for a source build, such as
//! "0.1.0 (git 5361244-dirty)". packaging/build-deb.sh sets PBC_RELEASE=1,
//! so packaged builds carry the plain version.

use std::process::Command;

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rerun-if-env-changed=PBC_RELEASE");

    let release = std::env::var_os("PBC_RELEASE").is_some_and(|v| !v.is_empty());
    let commit = if release {
        None
    } else {
        git(&dir, &["rev-parse", "--short", "HEAD"])
    };
    let full = match commit {
        Some(c) => {
            // Rebuild when HEAD moves or the working tree changes.
            if let Some(git_dir) = git(&dir, &["rev-parse", "--absolute-git-dir"]) {
                println!("cargo:rerun-if-changed={git_dir}/HEAD");
                println!("cargo:rerun-if-changed={git_dir}/index");
                if let Some(r) = git(&dir, &["symbolic-ref", "-q", "HEAD"]) {
                    println!("cargo:rerun-if-changed={git_dir}/{r}");
                }
            }
            let dirty = git(&dir, &["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            format!("{version} (git {c}{})", if dirty { "-dirty" } else { "" })
        }
        // A release, or a source tarball with no git.
        None => version,
    };
    println!("cargo:rustc-env=PBC_VERSION={full}");
}
