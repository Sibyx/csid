//! Build-time provenance for the session block (IP-139 Phase 3).
//!
//! Before this existed, `environment.csid_version` was the literal `0.1.0` in
//! `Cargo.toml`, never bumped. The daemon gained injection, time transfer,
//! segmentation, the BLE scanner and the empty-record counter inside that one
//! string, so the whole archive carries exactly one distinct value and a file's
//! provenance cannot distinguish a July capture from an August one.
//!
//! The semantic version stays the semantic version. What this script adds is
//! the *build identity* beside it, resolved in a strict order and **never
//! invented**: an absent revision is reported as absent, not guessed.
//!
//! ## Why a revision cannot always be read here
//!
//! The fleet build (`roles/csid/tasks/build.yml`) rsyncs the source to each node
//! with `--exclude=.git` and compiles there, so `git describe` has nothing to
//! read. That is deliberate — the nodes are not git clients. The control host
//! does have the checkout, so it can pass the identity in through
//! `CSID_BUILD_REVISION`, and the role already computes a deterministic source
//! content hash that is exactly the right value for it.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CSID_BUILD_REVISION");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let (revision, source) = resolve_revision();
    println!("cargo:rustc-env=CSID_BUILD_REVISION={revision}");
    println!("cargo:rustc-env=CSID_BUILD_REVISION_SOURCE={source}");

    // Seconds since the epoch, formatted at runtime by `util::rfc3339_utc`.
    // Calendar arithmetic lives in exactly one place in this workspace and a
    // build script is not going to be the second.
    println!("cargo:rustc-env=CSID_BUILD_EPOCH={}", build_epoch());

    println!("cargo:rustc-env=CSID_BUILD_RUSTC={}", rustc_version());
    println!(
        "cargo:rustc-env=CSID_BUILD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_default()
    );
}

/// Resolve the build revision and say where it came from.
///
/// `supplied` outranks `git` on purpose: a builder that names its own identity
/// knows something this script cannot work out, and the fleet is that builder.
fn resolve_revision() -> (String, &'static str) {
    if let Ok(v) = std::env::var("CSID_BUILD_REVISION") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return (v, "supplied");
        }
    }

    if let Some(v) = git_describe() {
        return (v, "git");
    }

    // Absent is a fact, and a different one from unknown-but-guessed.
    (String::new(), "none")
}

fn git_describe() -> Option<String> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);

    // Re-run when the checked-out commit moves, but only claim a dependency on
    // a path that exists — a rerun-if-changed on a missing file makes cargo
    // rebuild this crate on every single invocation.
    let git_dir = manifest.join("../../.git");
    for probe in ["HEAD", "refs"] {
        let p = git_dir.join(probe);
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }

    let out = Command::new("git")
        .args(["describe", "--always", "--dirty", "--tags"])
        .current_dir(&manifest)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// `SOURCE_DATE_EPOCH` when the builder pins it, otherwise now.
fn build_epoch() -> u64 {
    if let Ok(v) = std::env::var("SOURCE_DATE_EPOCH") {
        if let Ok(secs) = v.trim().parse::<u64>() {
            return secs;
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rustc_version() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
