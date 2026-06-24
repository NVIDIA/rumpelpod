// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Setup and installation of the rumpelpod daemon as a system service.
//! Supports systemd (Linux) and launchd (macOS).

use anyhow::{anyhow, Context, Result};
use indoc::formatdoc;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::daemon;

const SERVICE_NAME: &str = "rumpelpod";

fn check_docker_available() -> Result<()> {
    let found = Command::new("which")
        .arg("docker")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !found && !PathBuf::from("/usr/local/bin/docker").exists() {
        return Err(anyhow!(
            "docker not found in PATH or /usr/local/bin.\n\
             Docker is required for the rumpelpod daemon."
        ));
    }
    Ok(())
}

fn resolve_exe_path() -> Result<PathBuf> {
    let exe_path = std::env::current_exe().context("could not determine executable path")?;
    let display = exe_path.display();
    exe_path
        .canonicalize()
        .with_context(|| format!("could not resolve executable path: {display}"))
}

pub fn system_install() -> Result<()> {
    check_docker_available()?;

    if cfg!(target_os = "macos") {
        launchd_install()
    } else {
        systemd_install()
    }
}

pub fn system_uninstall() -> Result<()> {
    if cfg!(target_os = "macos") {
        launchd_uninstall()
    } else {
        systemd_uninstall()
    }
}

// ---------------------------------------------------------------------------
// launchd (macOS)
// ---------------------------------------------------------------------------

const LAUNCHD_LABEL: &str = "com.rumpelpod.daemon";

fn launchd_plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")))
}

fn launchd_plist_content() -> Result<String> {
    let exe_path = resolve_exe_path()?;
    let exe = exe_path.display();
    let socket = daemon::socket_path()?;
    let socket = socket.display();
    let label = LAUNCHD_LABEL;
    // Capture the installer's PATH so the daemon can find docker, git, etc.
    // even when they live outside the default launchd PATH (e.g. /opt/homebrew/bin).
    let path = std::env::var("PATH")
        .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string());

    Ok(formatdoc! {"
        <?xml version=\"1.0\" encoding=\"UTF-8\"?>
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
        <plist version=\"1.0\">
        <dict>
            <key>Label</key>
            <string>{label}</string>

            <key>ProgramArguments</key>
            <array>
                <string>{exe}</string>
                <string>daemon</string>
            </array>

            <key>EnvironmentVariables</key>
            <dict>
                <key>PATH</key>
                <string>{path}</string>
                <key>RUMPELPOD_DAEMON_SOCKET</key>
                <string>{socket}</string>
            </dict>

            <key>KeepAlive</key>
            <dict>
                <key>SuccessfulExit</key>
                <false/>
            </dict>

            <key>LimitLoadToSessionType</key>
            <array>
                <string>Aqua</string>
                <string>Background</string>
            </array>

            <key>RunAtLoad</key>
            <true/>

            <key>StandardOutPath</key>
            <string>/tmp/{label}.out.log</string>
            <key>StandardErrorPath</key>
            <string>/tmp/{label}.err.log</string>
        </dict>
        </plist>
    "})
}

/// Return the launchd domains to try, in order.
/// GUI domain (Aqua session) is preferred; user domain (Background session)
/// is the fallback for SSH-only environments.
fn launchd_domains() -> Vec<String> {
    let uid = unsafe { libc::getuid() };
    vec![format!("gui/{uid}"), format!("user/{uid}")]
}

/// Try to bootstrap the plist into the first available launchd domain.
fn launchd_bootstrap(plist: &str) -> Result<()> {
    for domain in launchd_domains() {
        // Unload first in case it was already loaded (ignore errors)
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, plist])
            .output();

        let output = Command::new("launchctl")
            .args(["bootstrap", &domain, plist])
            .output()
            .context("failed to run launchctl bootstrap")?;
        if output.status.success() {
            return Ok(());
        }
    }
    Err(anyhow!(
        "launchctl bootstrap failed for all domains (gui and user)"
    ))
}

/// Try to bootout the service from whichever domain it is loaded in.
fn launchd_bootout(plist: &str) {
    for domain in launchd_domains() {
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, plist])
            .output();
    }
}

fn launchd_install() -> Result<()> {
    let plist_path = launchd_plist_path()?;
    let plist_dir = plist_path
        .parent()
        .context("could not determine LaunchAgents directory")?;
    let plist_dir_display = plist_dir.display();
    fs::create_dir_all(plist_dir)
        .with_context(|| format!("failed to create {plist_dir_display}"))?;

    let content = launchd_plist_content()?;
    let display = plist_path.display();
    fs::write(&plist_path, &content).with_context(|| format!("failed to write {display}"))?;

    launchd_bootstrap(&plist_path.to_string_lossy())?;

    println!("installed rumpelpod daemon");
    Ok(())
}

fn launchd_uninstall() -> Result<()> {
    let plist_path = launchd_plist_path()?;

    launchd_bootout(&plist_path.to_string_lossy());

    if plist_path.exists() {
        let display = plist_path.display();
        fs::remove_file(&plist_path).with_context(|| format!("failed to remove {display}"))?;
    }

    println!("uninstalled rumpelpod daemon");
    Ok(())
}

// ---------------------------------------------------------------------------
// systemd (Linux)
// ---------------------------------------------------------------------------

fn systemd_user_dir() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("could not determine config directory")?;
    Ok(config_dir.join("systemd/user"))
}

fn socket_unit_path() -> Result<PathBuf> {
    Ok(systemd_user_dir()?.join(format!("{SERVICE_NAME}.socket")))
}

fn service_unit_path() -> Result<PathBuf> {
    Ok(systemd_user_dir()?.join(format!("{SERVICE_NAME}.service")))
}

fn socket_unit_content() -> Result<String> {
    let socket_path = daemon::socket_path()?;
    Ok(formatdoc! {"
        [Unit]
        Description=Rumpelpod daemon socket

        [Socket]
        ListenStream={socket_path}
        SocketMode=0600

        [Install]
        WantedBy=sockets.target
    ", socket_path = socket_path.display()})
}

fn service_unit_content() -> Result<String> {
    let exe_path = resolve_exe_path()?;
    let exe_path = exe_path.display();
    let service = SERVICE_NAME;

    Ok(formatdoc! {"
        [Unit]
        Description=Rumpelpod daemon for managing sandboxed LLM agents
        Requires={service}.socket

        [Service]
        Type=simple
        ExecStart={exe_path} daemon
        Restart=on-failure
        RestartSec=5

        [Install]
        WantedBy=default.target
    "})
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("failed to run systemctl")?;

    if !status.success() {
        let args = args.join(" ");
        return Err(anyhow!("systemctl --user {args} failed"));
    }
    Ok(())
}

fn check_systemd_available() -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .arg("--version")
        .output()
        .context("failed to run systemctl")?;

    if !output.status.success() {
        return Err(anyhow!(
            "systemd user session is not available.\n\
             Make sure you're running in a systemd-managed session."
        ));
    }
    Ok(())
}

fn systemd_install() -> Result<()> {
    check_systemd_available()?;

    let systemd_dir = systemd_user_dir()?;
    let socket_path = socket_unit_path()?;
    let service_path = service_unit_path()?;

    let dir = systemd_dir.display();
    fs::create_dir_all(&systemd_dir).with_context(|| format!("failed to create {dir}"))?;

    let socket_content = socket_unit_content()?;
    let service_content = service_unit_content()?;

    let socket_display = socket_path.display();
    fs::write(&socket_path, &socket_content)
        .with_context(|| format!("failed to write {socket_display}"))?;

    let service_display = service_path.display();
    fs::write(&service_path, &service_content)
        .with_context(|| format!("failed to write {service_display}"))?;

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", &format!("{SERVICE_NAME}.socket")])?;

    // Use restart rather than start so that reinstalls pick up the new binary.
    // Restarting the socket also stops the service (via Requires= dependency).
    systemctl(&["restart", &format!("{SERVICE_NAME}.socket")])?;

    println!("installed rumpelpod daemon");

    Ok(())
}

fn systemd_uninstall() -> Result<()> {
    check_systemd_available()?;

    let socket_path = socket_unit_path()?;
    let service_path = service_unit_path()?;

    let _ = systemctl(&["stop", &format!("{SERVICE_NAME}.socket")]);
    let _ = systemctl(&["stop", &format!("{SERVICE_NAME}.service")]);
    let _ = systemctl(&["disable", &format!("{SERVICE_NAME}.socket")]);

    if socket_path.exists() {
        let path = socket_path.display();
        fs::remove_file(&socket_path).with_context(|| format!("failed to remove {path}"))?;
    }

    if service_path.exists() {
        let path = service_path.display();
        fs::remove_file(&service_path).with_context(|| format!("failed to remove {path}"))?;
    }

    systemctl(&["daemon-reload"])?;

    println!("uninstalled rumpelpod daemon");

    Ok(())
}
