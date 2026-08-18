// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Browser integration coverage for the VS Code extension.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use indoc::{formatdoc, indoc};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{Pid, Uid};
use rumpelpod::CommandExt as RumpelCommandExt;

use crate::common::{
    create_commit, pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo,
};
use crate::executor::ExecutorResources;

const POD_NAME: &str = "vscode-review";
const CREATED_POD_NAME: &str = "vscode-created";
const CHANGED_FILE: &str = "browser-diff.txt";
const ORIGINAL_CONTENT: &str = "content from the host";
const POD_CONTENT: &str = "content from the pod";
const PREVIEW_CONTENT: &str = "rumpelpod forwarded preview";
const SECOND_PREVIEW_CONTENT: &str = "rumpelpod second forwarded preview";
const PREVIEW_PORT: u16 = 18765;
const SECOND_PREVIEW_PORT: u16 = 18766;
struct CodeServer {
    child: Child,
}

impl Drop for CodeServer {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => {
                eprintln!("checking code-server before cleanup failed: {error}");
            }
        }

        let pid = Pid::from_raw(self.child.id() as i32);
        if let Err(error) = kill(pid, Signal::SIGTERM) {
            eprintln!("stopping code-server failed: {error}");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => {
                    if let Err(error) = kill(pid, Signal::SIGKILL) {
                        eprintln!("killing code-server failed: {error}");
                    }
                    if let Err(error) = self.child.wait() {
                        eprintln!("waiting for killed code-server failed: {error}");
                    }
                    return;
                }
                Err(error) => {
                    eprintln!("checking code-server cleanup status failed: {error}");
                    return;
                }
            }
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root above rumpelpod crate")
        .to_path_buf()
}

fn executable_on_path(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is not set");
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} is not installed on PATH"))
}

fn find_vsix(root: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("RUMPELPOD_VSCODE_VSIX") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "RUMPELPOD_VSCODE_VSIX does not name a file: {}",
            path.display()
        );
        return path;
    }

    let path = root.join("vscode/dist/rumpelpod-vscode.vsix");
    assert!(
        path.is_file(),
        "the pipeline did not create its expected VSIX: {}",
        path.display()
    );
    path
}

fn reserve_loopback_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve code-server port");
    listener.local_addr().expect("read reserved port").port()
}

fn code_server_is_ready(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .is_err()
    {
        return false;
    }
    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return false;
    }
    let compact: String = response
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    response.starts_with("HTTP/1.1 200")
        && (compact.contains("\"status\":\"alive\"") || compact.contains("\"status\":\"expired\""))
}

fn wait_for_server(child: &mut Child, port: u16) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("check code-server status") {
            return Err(format!(
                "code-server exited before accepting connections: {status}"
            ));
        }
        if code_server_is_ready(port) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "code-server did not become healthy on port {port} within 30 seconds"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn install_extension(code_server: &Path, vsix: &Path, user_data_dir: &Path, extensions_dir: &Path) {
    let output = Command::new(code_server)
        .arg("--user-data-dir")
        .arg(user_data_dir)
        .arg("--extensions-dir")
        .arg(extensions_dir)
        .arg("--install-extension")
        .arg(vsix)
        .arg("--force")
        .output()
        .expect("run code-server extension installer");
    assert!(
        output.status.success(),
        "code-server failed to install {}:\nstdout:\n{}\nstderr:\n{}",
        vsix.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn start_code_server(
    code_server: &Path,
    repo: &TestRepo,
    daemon: &TestDaemon,
    user_data_dir: &Path,
    extensions_dir: &Path,
    action_log: &Path,
) -> (CodeServer, u16) {
    let ambient_path = std::env::var("PATH").expect("PATH is not set");
    let daemon_bin = daemon.bin_dir.display();
    let extension_path = format!("{daemon_bin}:{ambient_path}");

    let mut failures = Vec::new();
    for _attempt in 0..3 {
        let port = reserve_loopback_port();
        let address = format!("127.0.0.1:{port}");
        let mut command = Command::new(code_server);
        command
            .arg("--auth")
            .arg("none")
            .arg("--bind-addr")
            .arg(&address)
            .arg("--disable-telemetry")
            .arg("--disable-update-check")
            .arg("--disable-getting-started-override")
            .arg("--user-data-dir")
            .arg(user_data_dir)
            .arg("--extensions-dir")
            .arg(extensions_dir)
            .arg(repo.path())
            .env("HOME", &daemon.home_path)
            .env("PATH", &extension_path)
            .env("RUMPELPOD_DAEMON_SOCKET", &daemon.socket_path)
            .env(
                "RUMPELPOD_VSCODE_REAL_RUMPEL",
                daemon.bin_dir.join("rumpel"),
            )
            .env("RUMPELPOD_VSCODE_ACTION_LOG", action_log)
            .stdin(Stdio::null());
        let child = command
            .spawn_with_logging("CODE-SERVER")
            .expect("start code-server");
        let mut server = CodeServer { child };
        match wait_for_server(&mut server.child, port) {
            Ok(()) => return (server, port),
            Err(error) => failures.push(error),
        }
    }
    panic!("code-server failed to start after three attempts: {failures:?}")
}

fn write_code_server_settings(user_data_dir: &Path, executable: &Path) {
    let settings_dir = user_data_dir.join("User");
    fs::create_dir_all(&settings_dir).expect("create code-server settings directory");
    let settings = serde_json::json!({
        "editor.accessibilitySupport": "on",
        "security.workspace.trust.enabled": false,
        "telemetry.telemetryLevel": "off",
        "workbench.startupEditor": "none",
        "workbench.secondarySideBar.defaultVisibility": "hidden",
        "rumpelpod.executable": executable,
    });
    fs::write(
        settings_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).expect("serialize code-server settings"),
    )
    .expect("write code-server settings");
}

// TODO: Get rid of this wrapper and its fakes. Only real end-to-end
// behavior counts as tested: the merge should actually run (the pod
// needs to commit a DESCRIPTION, and the assertions must tolerate the
// workspace changing under code-server), and the action log should be
// replaced by asserting the commands' real effects.
fn write_extension_rumpel_wrapper(directory: &Path) -> PathBuf {
    let executable = directory.join("rumpel-vscode-test");
    fs::write(
        &executable,
        indoc! {r#"
            #!/bin/sh
            case "$1" in
                merge|stop|delete|ssh-add)
                    printf '%s\n' "$*" >> "$RUMPELPOD_VSCODE_ACTION_LOG"
                    ;;
            esac
            if [ "$1" = "merge" ]; then
                exit 0
            fi
            exec "$RUMPELPOD_VSCODE_REAL_RUMPEL" "$@"
        "#},
    )
    .expect("write VS Code rumpel wrapper");
    let mut permissions = fs::metadata(&executable)
        .expect("read VS Code rumpel wrapper metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).expect("make VS Code rumpel wrapper executable");
    executable
}

fn run_browser_assertions(
    root: &Path,
    port: u16,
    chromium: &Path,
    artifacts: &Path,
    repo: &TestRepo,
    daemon: &TestDaemon,
    action_log: &Path,
) {
    fs::create_dir_all(artifacts).expect("create browser artifact directory");
    let script = root.join("integration/vscode/browser.cjs");
    let base_url = format!("http://127.0.0.1:{port}");
    let status = Command::new("node")
        .arg(&script)
        .current_dir(root.join("vscode"))
        .env("RUMPELPOD_VSCODE_URL", base_url)
        .env("RUMPELPOD_VSCODE_POD", POD_NAME)
        .env("RUMPELPOD_VSCODE_CREATED_POD", CREATED_POD_NAME)
        .env("RUMPELPOD_VSCODE_CHANGED_FILE", CHANGED_FILE)
        .env("RUMPELPOD_VSCODE_ORIGINAL_CONTENT", ORIGINAL_CONTENT)
        .env("RUMPELPOD_VSCODE_POD_CONTENT", POD_CONTENT)
        .env("RUMPELPOD_VSCODE_PREVIEW_CONTENT", PREVIEW_CONTENT)
        .env(
            "RUMPELPOD_VSCODE_SECOND_PREVIEW_CONTENT",
            SECOND_PREVIEW_CONTENT,
        )
        .env("RUMPELPOD_VSCODE_REPO_ROOT", repo.path())
        .env("RUMPELPOD_VSCODE_RUMPEL", daemon.bin_dir.join("rumpel"))
        .env("RUMPELPOD_DAEMON_SOCKET", &daemon.socket_path)
        .env("RUMPELPOD_VSCODE_HOME", &daemon.home_path)
        .env("RUMPELPOD_CHROMIUM", chromium)
        .env("RUMPELPOD_VSCODE_ARTIFACTS", artifacts)
        .env("RUMPELPOD_VSCODE_ACTION_LOG", action_log)
        .status()
        .expect("run Playwright browser assertions");
    assert!(status.success(), "Playwright browser assertions failed");
}

#[test]
fn vscode_agent_environment_is_systemd_safe() {
    let root = workspace_root();
    let temporary = tempfile::tempdir().expect("create agent environment test directory");
    let source = temporary.path().join("process-environment");
    let destination = temporary.path().join("rumpelpod/agent-environment");
    fs::write(
        &source,
        b"UNRELATED=discard\0OPENAI_API_KEY=sentinel\"with\\chars\0",
    )
    .expect("write NUL-delimited process environment");
    let user = std::env::var("USER").expect("USER is not set");

    let destination_argument =
        format!("RUMPELPOD_AGENT_ENVIRONMENT_FILE={}", destination.display());
    Command::new("sudo")
        .args(["--non-interactive", "env", &destination_argument, "bash"])
        .arg(root.join(".devcontainer/write-agent-environment.sh"))
        .arg(&user)
        .arg(&source)
        .success()
        .expect("write protected agent environment");

    let content = fs::read_to_string(&destination).expect("read agent environment");
    assert_eq!(
        content, "OPENAI_API_KEY=\"sentinel\\\"with\\\\chars\"\n",
        "agent environment was not escaped for systemd"
    );
    assert!(
        !content.contains("UNRELATED"),
        "unapproved variable was written"
    );
    let mode = fs::metadata(&destination)
        .expect("read agent environment metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "agent environment was not private");
    let directory_owner = fs::metadata(destination.parent().expect("agent environment parent"))
        .expect("read agent environment directory metadata")
        .uid();
    assert_eq!(
        directory_owner,
        Uid::current().as_raw(),
        "root entrypoint setup left the user service directory under another owner"
    );
}

#[test]
fn vscode_devcontainer_defers_daemon_install_until_first_pipeline() {
    let root = workspace_root();
    // json5, not serde_json: devcontainer.json carries comments.
    let config: serde_json::Value = json5::from_str(
        &fs::read_to_string(root.join(".devcontainer/devcontainer.json"))
            .expect("read development container configuration"),
    )
    .expect("parse development container configuration");
    assert!(
        config.get("postCreateCommand").is_none(),
        "development services depended on a post-create lifecycle command"
    );

    let dockerfile = fs::read_to_string(root.join(".devcontainer/Dockerfile"))
        .expect("read development container Dockerfile");
    assert!(
        dockerfile.contains("touch /var/lib/systemd/linger/${USER}"),
        "the image did not arrange a user manager at container boot"
    );
    assert!(
        !dockerfile.contains(" AS ci")
            && !dockerfile.contains(" AS development")
            && dockerfile.contains("apt-get install --yes --no-install-recommends chromium"),
        "CI and development did not use the same complete devcontainer image"
    );
    assert!(
        !dockerfile.contains("system-install")
            && !dockerfile.contains(".devcontainer/rumpelpod.socket")
            && !dockerfile.contains(".devcontainer/rumpelpod.service "),
        "the image came with an installed rumpelpod daemon"
    );
    assert!(
        dockerfile.contains("ENV RUMPELPOD_DEVCONTAINER=1"),
        "the image did not mark the deferred daemon installation environment"
    );
    assert!(
        dockerfile.contains(
            "/home/${USER}/.config/systemd/user/default.target.wants/rumpelpod-vscode.service"
        ),
        "the image did not enable browser VS Code"
    );

    let entrypoint = fs::read_to_string(root.join(".devcontainer/entrypoint.sh"))
        .expect("read development container entrypoint");
    assert!(
        entrypoint.trim_end().ends_with("exec /sbin/init"),
        "the container entrypoint did not start systemd"
    );
    assert!(
        entrypoint.contains(r#"if [ -n "${CI:-}" ]"#)
            && entrypoint.contains(
                r#"ln -sf /dev/null "$USER_HOME/.config/systemd/user/rumpelpod-vscode.service""#
            ),
        "CI containers did not mask browser VS Code"
    );
    let ci_pipeline =
        fs::read_to_string(root.join("ci/run-pipeline.sh")).expect("read CI pipeline script");
    assert!(
        ci_pipeline.contains("--env CI"),
        "CI did not forward its marker into the test container"
    );
    let cargo_pipeline =
        fs::read_to_string(root.join("tools/src/bin/pipeline.rs")).expect("read Cargo pipeline");
    let build = cargo_pipeline
        .find(".args([\"build\", \"--all-targets\"])")
        .expect("pipeline did not build workspace binaries");
    let install = cargo_pipeline
        .find("install_devcontainer_daemon(release)?")
        .expect("pipeline did not install the development daemon");
    assert!(
        build < install
            && cargo_pipeline.contains("const DEVCONTAINER_ENV: &str = \"RUMPELPOD_DEVCONTAINER\"")
            && cargo_pipeline.contains("rumpel-linux-amd64")
            && cargo_pipeline.contains("rumpel-linux-arm64")
            && cargo_pipeline.contains("--user\", \"show-environment")
            && cargo_pipeline.contains(".arg(\"system-install\")"),
        "the first marked pipeline did not install its freshly built daemon and payloads"
    );
}

#[test]
fn vscode_package_is_native_and_published_for_each_release_platform() {
    let root = workspace_root();
    let package: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("vscode/package.json")).expect("read extension manifest"),
    )
    .expect("parse extension manifest");
    let dependencies = package["devDependencies"]
        .as_object()
        .expect("extension devDependencies object");
    assert!(
        dependencies.contains_key("node-pty"),
        "the extension did not declare its native PTY module"
    );

    let package_script =
        fs::read_to_string(root.join("vscode/package.mjs")).expect("read extension package script");
    assert!(
        package_script.contains("--target"),
        "the extension package did not declare its native platform"
    );
    let build_script =
        fs::read_to_string(root.join("vscode/esbuild.mjs")).expect("read extension build script");
    assert!(
        build_script.contains("spawn-helper")
            && build_script.contains("fs.chmod")
            && build_script.contains("0o755"),
        "the macOS PTY helper was not staged as an executable"
    );

    let vsix = find_vsix(&root);
    let entries = Command::new("unzip")
        .args(["-Z1"])
        .arg(&vsix)
        .success()
        .expect("list packaged extension files");
    let entries = String::from_utf8(entries).expect("VSIX entries were not UTF-8");
    assert!(
        entries.lines().any(|entry| entry.ends_with(".node")),
        "the extension package omitted its native Node module"
    );
    assert!(
        entries.contains("node-pty"),
        "the extension package omitted the PTY runtime"
    );
    assert!(
        !entries.lines().any(|entry| entry.ends_with(".test.js")),
        "the extension package included node-pty's test suite"
    );
    assert!(
        !entries.lines().any(|entry| entry.ends_with(".map")),
        "the extension package included production source maps"
    );

    let manifest = Command::new("unzip")
        .args(["-p"])
        .arg(&vsix)
        .arg("extension.vsixmanifest")
        .success()
        .expect("read packaged extension manifest");
    assert!(
        String::from_utf8(manifest)
            .expect("VSIX manifest was not UTF-8")
            .contains("TargetPlatform"),
        "the VSIX did not declare a platform target"
    );

    let workflow =
        fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("read release workflow");
    for target in ["linux-x64", "linux-arm64", "darwin-arm64"] {
        assert!(
            workflow.contains(&format!(
                "rumpelpod-vscode-${{{{ github.ref_name }}}}-{target}.vsix"
            )),
            "tagged releases did not publish the {target} VSIX"
        );
    }
    let ci_script =
        fs::read_to_string(root.join("ci/run-pipeline.sh")).expect("read CI pipeline script");
    assert!(
        !ci_script.contains("--target ci")
            && ci_script.contains("vscode/Dockerfile.linux")
            && ci_script.contains("type=local,dest=$vsix_output"),
        "Linux release jobs did not use the compatibility VSIX builder"
    );
    assert!(
        ci_script.contains("docker inspect --format '{{.State.Running}}'")
            && ci_script.contains("docker logs devcontainer"),
        "CI did not report devcontainer startup failures"
    );
    let linux_builder =
        fs::read_to_string(root.join("vscode/Dockerfile.linux")).expect("read Linux VSIX builder");
    assert!(
        linux_builder.contains("snapshot.debian.org")
            && linux_builder.contains("dpkg --compare-versions")
            && linux_builder.contains("le 2.31"),
        "Linux VSIX releases did not pin and enforce their glibc baseline"
    );
    let installer = fs::read_to_string(root.join("install.sh")).expect("read installer");
    assert!(
        installer.contains("releases/download/${version}/${tarball}")
            && installer.contains("tar xzf"),
        "the VSIX release changed the existing binary tarball installer"
    );
}

#[test]
fn vscode_development_dependencies_follow_lockfile_policy() {
    let root = workspace_root();
    let package: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("vscode/package.json")).expect("read extension manifest"),
    )
    .expect("parse extension manifest");
    let dependencies = package["devDependencies"]
        .as_object()
        .expect("extension devDependencies object");
    for (name, requested) in dependencies {
        let expected = match name.as_str() {
            "@types/vscode" => "1.109.0",
            "@playwright/test" | "@types/node" | "@vscode/vsce" | "@xterm/addon-fit"
            | "@xterm/xterm" | "esbuild" | "node-pty" | "typescript" => "latest",
            dependency => panic!("unexpected extension development dependency: {dependency}"),
        };
        assert_eq!(
            requested.as_str(),
            Some(expected),
            "{name} did not use the expected stable dependency selector"
        );
    }
    assert_eq!(
        package["engines"]["vscode"].as_str(),
        Some("^1.109.0"),
        "the extension engine changed without its API types"
    );

    let lock: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("vscode/package-lock.json"))
            .expect("read extension lockfile"),
    )
    .expect("parse extension lockfile");
    assert_eq!(
        lock["packages"][""]["devDependencies"], package["devDependencies"],
        "the lockfile root did not preserve the manifest selectors"
    );
    let locked_packages = lock["packages"]
        .as_object()
        .expect("extension lockfile packages object");
    for name in dependencies.keys() {
        let path = format!("node_modules/{name}");
        let version = locked_packages
            .get(&path)
            .and_then(|entry| entry["version"].as_str())
            .unwrap_or_else(|| panic!("lockfile did not pin {name}"));
        assert!(
            version
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_digit()),
            "lockfile version for {name} was not concrete: {version}"
        );
    }
    assert_eq!(
        locked_packages["node_modules/@types/vscode"]["version"].as_str(),
        Some("1.109.0"),
        "the locked VS Code API types did not match the supported editor"
    );
}

#[test]
fn vscode_server_is_unauthenticated_and_loopback_only() {
    let root = workspace_root();
    let temporary = tempfile::tempdir().expect("create VS Code service test directory");
    let home = temporary.path().join("home");
    let bin_dir = home.join(".local/bin");
    let capture = temporary.path().join("code-server-arguments");
    let code_server = bin_dir.join("code-server");
    fs::create_dir_all(&bin_dir).expect("create fake code-server directory");
    fs::write(
        &code_server,
        indoc! {r#"
            #!/bin/sh
            set -eu
            if [ "${PASSWORD+x}" = x ] || [ "${HASHED_PASSWORD+x}" = x ]; then
                exit 91
            fi
            printf '%s\n' "$@" > "$CAPTURE"
        "#},
    )
    .expect("write fake code-server");
    fs::set_permissions(&code_server, fs::Permissions::from_mode(0o755))
        .expect("make fake code-server executable");

    Command::new("sh")
        .arg(root.join(".devcontainer/start-vscode.sh"))
        .env("HOME", &home)
        .env("CAPTURE", &capture)
        .env("PASSWORD", "must-not-reach-code-server")
        .env("HASHED_PASSWORD", "must-not-reach-code-server")
        .success()
        .expect("start loopback-only code-server service");

    let arguments = fs::read_to_string(&capture).expect("read code-server arguments");
    let workspace = Path::new("/workspaces/anyhow-demo");
    let workspace = workspace.display();
    assert_eq!(
        arguments,
        formatdoc! {"
            --auth
            none
            --bind-addr
            127.0.0.1:3000
            --disable-telemetry
            --disable-update-check
            --disable-workspace-trust
            {workspace}
        "},
        "the browser service accepted authentication or network overrides"
    );
    let service = fs::read_to_string(root.join(".devcontainer/rumpelpod-vscode.service"))
        .expect("read VS Code service");
    assert!(
        service.contains("ConditionPathIsDirectory=/workspaces/anyhow-demo/.git")
            && service.contains("WorkingDirectory=/workspaces/anyhow-demo")
            && !service.contains("After=rumpelpod.socket")
            && !service.contains("Requires=rumpelpod.socket"),
        "the browser service could not start before the first daemon install"
    );
}

#[test]
fn vscode_demo_workspace_uses_standard_runtime() {
    let root = workspace_root();
    let temporary = tempfile::tempdir().expect("create VS Code demo test directory");
    let home = temporary.path().join("home");
    let source = temporary.path().join("cached-anyhow");
    let workspace = temporary.path().join("workspaces/anyhow-demo");
    fs::create_dir_all(source.join("src")).expect("create cached anyhow source");
    fs::write(
        source.join("Cargo.toml"),
        indoc! {r#"
            [package]
            name = "anyhow"
            version = "1.0.102"
            edition = "2021"
        "#},
    )
    .expect("write cached anyhow manifest");
    fs::write(source.join("src/lib.rs"), "pub struct Error;\n")
        .expect("write cached anyhow source");
    fs::write(source.join(".cargo-ok"), "").expect("write registry marker");
    fs::write(source.join(".cargo_vcs_info.json"), "{}").expect("write registry metadata");

    let prepare = root.join(".devcontainer/prepare-vscode-demo.sh");
    Command::new("sh")
        .arg(&prepare)
        .env("HOME", &home)
        .env("RUMPELPOD_VSCODE_DEMO_SOURCE", &source)
        .env("RUMPELPOD_VSCODE_WORKSPACE", &workspace)
        .success()
        .expect("seed VS Code demo workspace");

    let config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".devcontainer/devcontainer.json"))
            .expect("read demo devcontainer"),
    )
    .expect("parse demo devcontainer");
    assert_eq!(config["workspaceFolder"], "/workspace/anyhow");
    assert_eq!(config["containerUser"], "user");
    assert!(
        config.get("runArgs").is_none(),
        "demo devcontainer requested an outer-container runtime"
    );
    assert_eq!(config["userEnvProbe"], "none");
    assert_eq!(config["forwardPorts"], serde_json::json!([8000, 8001]));
    assert_eq!(
        config["portsAttributes"]["8000"]["label"],
        "Anyhow primary preview"
    );
    assert_eq!(config["portsAttributes"]["8000"]["protocol"], "http");
    assert_eq!(
        config["portsAttributes"]["8000"]["onAutoForward"],
        "openPreview"
    );
    assert_eq!(
        config["portsAttributes"]["8001"],
        serde_json::json!({
            "label": "Anyhow secondary preview",
            "protocol": "http",
            "onAutoForward": "openPreview"
        })
    );
    assert_eq!(
        config["postStartCommand"]["primaryPreview"],
        "RUMPELPOD_PREVIEW_PORT=8000 RUMPELPOD_PREVIEW_PAGE=index.html sh \
         .devcontainer/start-preview.sh"
    );
    assert_eq!(
        config["postStartCommand"]["secondaryPreview"],
        "RUMPELPOD_PREVIEW_PORT=8001 RUMPELPOD_PREVIEW_PAGE=secondary.html sh \
         .devcontainer/start-preview.sh"
    );
    assert_eq!(config["waitFor"], "postStartCommand");
    let dockerfile = fs::read_to_string(workspace.join(".devcontainer/Dockerfile"))
        .expect("read demo Dockerfile");
    assert!(
        dockerfile.contains(
            "rust:1.96.1-slim-bookworm@sha256:e18a79fc84dfcfc3ab5ba72290398a644c135c97eaa881447fddc354ee4701a3"
        ),
        "demo Rust image was not pinned"
    );
    assert!(
        dockerfile.contains("useradd -m -s /bin/bash user"),
        "demo pods must not run agents as root; Claude rejects \
         --dangerously-skip-permissions under root"
    );
    assert!(
        dockerfile.contains("git python3 sudo"),
        "demo image did not include the preview server runtime"
    );
    assert!(
        !dockerfile.contains("sysbox"),
        "demo Dockerfile requested Sysbox"
    );
    let preview = fs::read_to_string(workspace.join(".devcontainer/preview.py"))
        .expect("read demo preview server");
    assert!(
        preview.contains("ThreadingHTTPServer((\"0.0.0.0\", PREVIEW_PORT)"),
        "preview server was not reachable through the forwarded port"
    );
    let preview_start = fs::read_to_string(workspace.join(".devcontainer/start-preview.sh"))
        .expect("read demo preview startup script");
    assert!(
        preview_start.contains("if preview_is_ready; then"),
        "preview startup was not idempotent"
    );
    let preview_page = fs::read_to_string(workspace.join(".devcontainer/index.html"))
        .expect("read demo preview page");
    assert!(
        preview_page.contains("Anyhow pod preview"),
        "demo preview page was missing"
    );
    let secondary_page = fs::read_to_string(workspace.join(".devcontainer/secondary.html"))
        .expect("read secondary demo preview page");
    assert!(
        secondary_page.contains("Secondary anyhow preview"),
        "secondary demo preview page was missing"
    );
    let preview_port = reserve_loopback_port();
    let mut secondary_preview_port = reserve_loopback_port();
    while secondary_preview_port == preview_port {
        secondary_preview_port = reserve_loopback_port();
    }
    let preview_state_dir = temporary.path().join("preview-state");
    fs::create_dir_all(&preview_state_dir).expect("create preview server state directory");
    Command::new("sh")
        .arg("-c")
        .arg(indoc! {r#"
            set -eu
            pid_file="$RUMPELPOD_PREVIEW_STATE_DIR/anyhow-preview-$RUMPELPOD_PREVIEW_PORT.pid"
            cleanup() {
                cleanup_status=0
                for cleanup_port in \
                    "$RUMPELPOD_PREVIEW_PORT" \
                    "$RUMPELPOD_SECONDARY_PREVIEW_PORT"
                do
                    cleanup_pid_file="$RUMPELPOD_PREVIEW_STATE_DIR/anyhow-preview-$cleanup_port.pid"
                    if [ ! -f "$cleanup_pid_file" ]; then
                        continue
                    fi
                    cleanup_pid=$(cat "$cleanup_pid_file")
                    if [ "$cleanup_pid" = "$$" ]; then
                        continue
                    fi
                    if ! kill "$cleanup_pid" 2>/dev/null; then
                        echo "could not stop test preview server PID $cleanup_pid" >&2
                        cleanup_status=1
                    fi
                done
                return "$cleanup_status"
            }
            trap cleanup EXIT

            sh .devcontainer/start-preview.sh
            first_pid=$(cat "$pid_file")
            sh .devcontainer/start-preview.sh
            second_pid=$(cat "$pid_file")
            if [ "$first_pid" != "$second_pid" ]; then
                echo "second startup replaced preview server PID $first_pid with $second_pid" >&2
                exit 1
            fi
            python3 -c 'import os, urllib.request; port = os.environ["RUMPELPOD_PREVIEW_PORT"]; content = urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=1).read(); assert b"Anyhow pod preview" in content'

            RUMPELPOD_PREVIEW_PORT="$RUMPELPOD_SECONDARY_PREVIEW_PORT" \
                RUMPELPOD_PREVIEW_PAGE=secondary.html \
                sh .devcontainer/start-preview.sh
            secondary_pid_file="$RUMPELPOD_PREVIEW_STATE_DIR/anyhow-preview-$RUMPELPOD_SECONDARY_PREVIEW_PORT.pid"
            secondary_pid=$(cat "$secondary_pid_file")
            python3 -c 'import os, urllib.request; port = os.environ["RUMPELPOD_SECONDARY_PREVIEW_PORT"]; content = urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=1).read(); assert b"Secondary anyhow preview" in content'

            kill "$first_pid"
            attempts=0
            while kill -0 "$first_pid" 2>/dev/null && [ "$attempts" -lt 50 ]; do
                attempts=$((attempts + 1))
                sleep 0.1
            done
            if kill -0 "$first_pid" 2>/dev/null; then
                echo "preview server PID $first_pid did not stop" >&2
                exit 1
            fi
            printf '%s\n' "$secondary_pid" > "$pid_file"
            sh .devcontainer/start-preview.sh
            restarted_pid=$(cat "$pid_file")
            if [ "$restarted_pid" = "$secondary_pid" ]; then
                echo "primary startup trusted the secondary preview server PID" >&2
                exit 1
            fi
            python3 -c 'import os, urllib.request; port = os.environ["RUMPELPOD_PREVIEW_PORT"]; content = urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=1).read(); assert b"Anyhow pod preview" in content'
        "#})
        .env("RUMPELPOD_PREVIEW_PORT", preview_port.to_string())
        .env(
            "RUMPELPOD_SECONDARY_PREVIEW_PORT",
            secondary_preview_port.to_string(),
        )
        .env("RUMPELPOD_PREVIEW_STATE_DIR", &preview_state_dir)
        .current_dir(&workspace)
        .success()
        .expect("restart the generated preview server and request its page");
    assert!(!workspace.join(".cargo-ok").exists());
    assert!(!workspace.join(".cargo_vcs_info.json").exists());
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&workspace)
        .success()
        .expect("read demo repository status");
    assert!(status.is_empty(), "seeded demo repository was dirty");
    let user_name = Command::new("git")
        .args(["config", "--local", "user.name"])
        .current_dir(&workspace)
        .success()
        .expect("read demo repository Git user name");
    assert_eq!(user_name, b"Rumpelpod VS Code\n");
    let user_email = Command::new("git")
        .args(["config", "--local", "user.email"])
        .current_dir(&workspace)
        .success()
        .expect("read demo repository Git email");
    assert_eq!(user_email, b"rumpelpod-vscode@localhost\n");

    let marker = workspace.join("developer-work");
    fs::write(&marker, "preserve me\n").expect("write demo developer work");
    Command::new("sh")
        .arg(&prepare)
        .env("HOME", &home)
        .env("RUMPELPOD_VSCODE_DEMO_SOURCE", &source)
        .env("RUMPELPOD_VSCODE_WORKSPACE", &workspace)
        .success()
        .expect("re-run VS Code demo preparation");
    assert_eq!(
        fs::read_to_string(marker).expect("read preserved demo work"),
        "preserve me\n",
        "live extension rebuild replaced the demo working tree"
    );
}

#[test]
#[ignore = "slow and timing-sensitive browser coverage; run explicitly"]
fn vscode_browser_lists_creates_and_reviews_pods() {
    let root = workspace_root();
    let code_server = executable_on_path("code-server");
    let chromium = executable_on_path("chromium");
    let vsix = find_vsix(&root);

    let repo = TestRepo::new();
    fs::write(
        repo.path().join(CHANGED_FILE),
        format!("{ORIGINAL_CONTENT}\n"),
    )
    .expect("write initial file");
    fs::write(
        repo.path().join("index.html"),
        format!("<!doctype html><title>Rumpelpod preview</title>{PREVIEW_CONTENT}\n"),
    )
    .expect("write forwarded preview fixture");
    Command::new("git")
        .args(["add", CHANGED_FILE, "index.html"])
        .current_dir(repo.path())
        .success()
        .expect("stage initial file");
    create_commit(repo.path(), "Add browser diff fixture");

    let home = TestHome::new();
    crate::codex::common::setup_controlled_home(&home);
    home.link_local_bins(&["ssh-agent", "ssh-add"]);
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start_with_local_llm_clis(&home);
    let port_config = formatdoc! {r#",
        "forwardPorts": [{PREVIEW_PORT}, {SECOND_PREVIEW_PORT}],
        "portsAttributes": {{
            "{PREVIEW_PORT}": {{
                "label": "Browser fixture",
                "protocol": "http",
                "onAutoForward": "openPreview"
            }},
            "{SECOND_PREVIEW_PORT}": {{
                "label": "Second browser fixture",
                "protocol": "http",
                "onAutoForward": "openPreview"
            }}
        }}
    "#};
    write_test_devcontainer(&repo, "RUN apk add --no-cache socat\n", &port_config);
    fs::write(repo.path().join(".rumpelpod.json"), &executor.json).expect("write rumpelpod config");

    pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            POD_NAME,
            "--",
            "sh",
            "-c",
            &format!(
                "nohup sh -c 'while true; do printf \"HTTP/1.1 200 OK\\r\\nContent-Length: {}\\r\\nConnection: close\\r\\n\\r\\n{}\" | socat - TCP-LISTEN:{PREVIEW_PORT},reuseaddr; done' >/tmp/rumpelpod-preview.log 2>&1 & nohup sh -c 'while true; do printf \"HTTP/1.1 200 OK\\r\\nContent-Length: {}\\r\\nConnection: close\\r\\n\\r\\n{}\" | socat - TCP-LISTEN:{SECOND_PREVIEW_PORT},reuseaddr; done' >/tmp/rumpelpod-second-preview.log 2>&1 &",
                PREVIEW_CONTENT.len(),
                PREVIEW_CONTENT,
                SECOND_PREVIEW_CONTENT.len(),
                SECOND_PREVIEW_CONTENT,
            ),
        ])
        .success()
        .expect("launch test pod");

    let browser_home = tempfile::tempdir().expect("create code-server home");
    let user_data_dir = browser_home.path().join("user-data");
    let extensions_dir = browser_home.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("create code-server extension directory");
    let extension_rumpel = write_extension_rumpel_wrapper(browser_home.path());
    let action_log = browser_home.path().join("actions.log");
    let ssh_directory = daemon.home_path.join(".ssh");
    fs::create_dir_all(&ssh_directory).expect("create SSH key directory");
    let ssh_key = ssh_directory.join("id-vscode-test");
    Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(&ssh_key)
        .args(["-N", "vscode-passphrase", "-q"])
        .success()
        .expect("generate SSH key picker fixture");
    write_code_server_settings(&user_data_dir, &extension_rumpel);
    install_extension(&code_server, &vsix, &user_data_dir, &extensions_dir);

    let (_code_server, port) = start_code_server(
        &code_server,
        &repo,
        &daemon,
        &user_data_dir,
        &extensions_dir,
        &action_log,
    );
    let artifacts = root
        .join("target/vscode-integration")
        .join(std::process::id().to_string());
    run_browser_assertions(
        &root,
        port,
        &chromium,
        &artifacts,
        &repo,
        &daemon,
        &action_log,
    );
    let ssh_identities = pod_command(&repo, &daemon)
        .args(["ssh-add", POD_NAME, "-l"])
        .success()
        .expect("list identities added by the VS Code extension");
    assert!(
        String::from_utf8_lossy(&ssh_identities).contains("ED25519"),
        "VS Code did not add the selected SSH identity"
    );
}
