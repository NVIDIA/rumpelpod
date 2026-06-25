// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Verify that prepared-image setup can install pi from the client-resolved
//! host CLI version, without relying on pi already being in the base image.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::common::{pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo};
use crate::executor::ExecutorResources;
use rumpelpod::CommandExt;

use super::common::PI_VERSION;

fn write_fake_host_pi(home: &TestHome) -> PathBuf {
    let dir = home.path().join("client-only-pi-bin");
    fs::create_dir_all(&dir).expect("create client-only-pi-bin");
    let path = dir.join("pi");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then\n\
             \techo '{PI_VERSION}'\n\
             \texit 0\n\
             fi\n\
             echo 'fake pi only supports --version' >&2\n\
             exit 1\n"
        ),
    )
    .expect("write fake pi");
    let mut permissions = fs::metadata(&path).expect("stat fake pi").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod fake pi");
    dir
}

#[test]
fn image_includes_pi_from_client_path() {
    println!("xtest:timeout=240");

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    let client_only = write_fake_host_pi(&home);
    let client_path = format!("{}:{}", client_only.display(), daemon.bin_dir.display());

    let stdout = pod_command(&repo, &daemon)
        .env("PATH", client_path)
        .args([
            "enter",
            "--create",
            "pi-install-test",
            "--",
            "/opt/rumpelpod/bin/pi",
            "--version",
        ])
        .success()
        .expect("pi binary should run in the container");
    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        stdout.contains(PI_VERSION),
        "pi --version should contain {PI_VERSION}, stdout: {stdout}",
    );
}
