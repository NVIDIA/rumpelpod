// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

use sha2::{Digest, Sha256};

fn git_output(args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {}: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("non-UTF-8 git output")
        .trim()
        .to_string()
}

fn main() {
    let commit = git_output(&["rev-parse", "HEAD"]);
    let date = git_output(&["log", "-1", "--format=%cd", "--date=short", "HEAD"]);

    let version = Command::new("git")
        .args(["describe", "--tags", "--exact-match", "HEAD"])
        .output()
        .expect("failed to run git describe");
    let version = if version.status.success() {
        String::from_utf8(version.stdout)
            .expect("non-UTF-8 git output")
            .trim()
            .to_string()
    } else {
        "v0.0.0".to_string()
    };

    let diff = git_output(&["diff", "HEAD"]);

    let untracked_output = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .expect("failed to run git ls-files");
    assert!(
        untracked_output.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&untracked_output.stderr)
    );
    let untracked_list = String::from_utf8(untracked_output.stdout)
        .expect("non-UTF-8 git output")
        .trim()
        .to_string();

    let dirty_suffix = if !diff.is_empty() || !untracked_list.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(diff.as_bytes());
        for path in untracked_list.lines() {
            hasher.update(path.as_bytes());
            if let Ok(contents) = std::fs::read(path) {
                hasher.update(&contents);
            }
        }
        let hash = hex::encode(hasher.finalize());
        format!("+{}", &hash[..40])
    } else {
        String::new()
    };

    println!("cargo:rustc-env=RUMPELPOD_VERSION_INFO={version} {date} {commit}{dirty_suffix}");
}
