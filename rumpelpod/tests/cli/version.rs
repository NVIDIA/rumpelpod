// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

#[test]
fn version_output_format() {
    let output = Command::new("rumpel")
        .arg("--version")
        .output()
        .expect("failed to run rumpel --version");

    assert!(
        output.status.success(),
        "rumpel --version failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("non-UTF-8 output");
    let stdout = stdout.trim_end();
    let fields: Vec<&str> = stdout.split_whitespace().collect();
    assert_eq!(fields.len(), 4, "expected 4 fields, got: {stdout:?}");

    assert_eq!(fields[0], "rumpelpod");

    // The version is the release tag name, which carries a leading v by
    // convention (v0.4.0); untagged builds embed v0.0.0.
    let version = fields[1]
        .strip_prefix('v')
        .unwrap_or_else(|| panic!("version missing v prefix: {}", fields[1]));
    let version_parts: Vec<&str> = version.split('.').collect();
    assert_eq!(version_parts.len(), 3, "version not semver: {version}");
    for part in &version_parts {
        part.parse::<u32>()
            .unwrap_or_else(|_| panic!("version component not a number: {part}"));
    }

    let date = fields[2];
    assert_eq!(date.len(), 10, "date wrong length: {date}");
    let dashes: Vec<_> = date.match_indices('-').collect();
    assert_eq!(dashes.len(), 2, "date missing dashes: {date}");
    assert_eq!(dashes[0].0, 4, "date first dash wrong position: {date}");
    assert_eq!(dashes[1].0, 7, "date second dash wrong position: {date}");

    let commit_field = fields[3];
    let (hash, dirty) = match commit_field.split_once('+') {
        Some((h, d)) => (h, Some(d)),
        None => (commit_field, None),
    };
    assert_eq!(hash.len(), 40, "commit hash wrong length: {hash}");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "commit hash not hex: {hash}"
    );
    if let Some(dirty_hash) = dirty {
        assert_eq!(
            dirty_hash.len(),
            40,
            "dirty hash wrong length: {dirty_hash}"
        );
        assert!(
            dirty_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "dirty hash not hex: {dirty_hash}"
        );
    }
}
