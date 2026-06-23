// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Verify that every source file carries the SPDX license header.
//!
//! Sits alongside the THIRD-PARTY-NOTICES check and serves the same purpose:
//! it keeps the repository's license metadata honest so that a new source
//! file cannot be merged without attribution. The two required lines are
//! printed for any offending file.

use std::path::{Path, PathBuf};
use std::process::Command;

// The header is matched loosely so the test enforces presence rather than
// exact wording: the copyright year may differ on files added in later years,
// and the comment prefix differs between languages (`//` vs `#`).
const COPYRIGHT_MARKER: &str = "SPDX-FileCopyrightText:";
const COPYRIGHT_HOLDER: &str = "NVIDIA CORPORATION & AFFILIATES";
const LICENSE_LINE: &str = "SPDX-License-Identifier: Apache-2.0";

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is the rumpelpod crate; workspace root is one up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root above crate dir")
        .to_path_buf()
}

// File types whose comment syntax lets them carry the header. Documentation
// (Markdown), license texts and comment-less formats (JSON) are deliberately
// excluded; everything listed here is expected to have the header.
fn requires_header(path: &Path) -> bool {
    if path.file_name().and_then(|n| n.to_str()) == Some("Dockerfile") {
        return true;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rs" | "sh" | "toml" | "yml" | "yaml")
    )
}

#[test]
fn source_files_have_license_header() {
    let root = workspace_root();

    // git ls-files enumerates tracked files only, so generated output under
    // target/ and untracked scratch files never reach the check.
    let output = Command::new("git")
        .current_dir(&root)
        .args(["ls-files", "-z"])
        .output()
        .expect("running git ls-files");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("git ls-files failed:\n{stderr}");
    }

    let mut missing = Vec::new();
    for rel in output.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        let rel = std::str::from_utf8(rel).expect("tracked file path is utf8");
        let path = root.join(rel);
        if !requires_header(&path) {
            continue;
        }

        let contents =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {rel}: {e}"));
        // The header sits at the very top, after an optional shebang line, so
        // only the first few lines need inspecting.
        let head = contents.lines().take(5).collect::<Vec<_>>().join("\n");
        let has_copyright = head.contains(COPYRIGHT_MARKER) && head.contains(COPYRIGHT_HOLDER);
        let has_license = head.contains(LICENSE_LINE);
        if !has_copyright || !has_license {
            missing.push(rel.to_string());
        }
    }

    if !missing.is_empty() {
        let list = missing.join("\n  ");
        panic!(
            "The following source files are missing the SPDX license header:\n  {list}\n\n\
             Add these two lines at the top (after any shebang), using the file's comment prefix:\n  \
             {COPYRIGHT_MARKER} Copyright (c) 2026 {COPYRIGHT_HOLDER}. All rights reserved.\n  \
             {LICENSE_LINE}"
        );
    }
}
