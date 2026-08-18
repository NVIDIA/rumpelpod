// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for SSH remote Docker functionality.
//!
//! These tests verify that pods can be created on remote Docker hosts
//! accessed via SSH. The tests start a Docker container that runs
//! both an SSH server and a Docker daemon (in privileged mode), simulating
//! a remote Docker host.

use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indoc::{formatdoc, indoc};
use tempfile::TempDir;

use crate::common::{
    pod_command, write_test_devcontainer, TestDaemon, TestHome, TestRepo, TEST_REPO_PATH,
    TEST_USER_UID,
};
use crate::executor::ExecutorResources;
use rumpelpod::CommandExt;

/// Test user for SSH connections.
pub const SSH_USER: &str = "testuser";

/// Timeout for waiting for services to become available.
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);

const NESTED_DOCKER_DNS_ENV: &str = "RUMPELPOD_NESTED_DOCKER_DNS";

/// Which container engine the remote host container runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RemoteEngine {
    Docker,
    Podman,
}

/// A container simulating a remote Docker or Podman host with SSH access.
///
/// This is a test fixture similar to `TestRepo` and `TestDaemon`.
/// It manages a Docker container running an SSH server plus either a
/// Docker daemon or a rootful Podman API service.
///
/// On drop, the container is stopped and removed.
pub struct SshRemoteHost {
    /// Docker container ID.
    container_id: String,
    /// The engine served by this remote host.
    engine: RemoteEngine,
    /// IP address of the container (used on Linux for direct connection).
    ip_address: String,
    /// Published SSH port on localhost (used on macOS Docker Desktop where
    /// container IPs are not routable from the host).
    published_port: Option<u16>,
    /// Temporary directory containing SSH keys.
    _temp_dir: TempDir,
    /// Path to the private key file.
    private_key_path: PathBuf,
    /// Docker network name.
    network_name: String,
}

impl SshRemoteHost {
    /// Start a new SSH remote host container.
    ///
    /// This builds (if necessary) and starts a container with SSH and Docker,
    /// generates SSH keys, and configures the container to accept connections.
    pub fn start() -> Self {
        Self::start_inner(false, RemoteEngine::Docker)
    }

    /// Start a new SSH remote host container with SSH published on localhost.
    pub fn start_published() -> Self {
        Self::start_inner(true, RemoteEngine::Docker)
    }

    /// Start a new SSH remote host container serving a Podman API socket.
    pub fn start_podman() -> Self {
        Self::start_inner(false, RemoteEngine::Podman)
    }

    fn start_inner(force_publish_ssh: bool, engine: RemoteEngine) -> Self {
        let image_id = match engine {
            RemoteEngine::Docker => {
                build_remote_docker_image().expect("Failed to build remote docker image")
            }
            RemoteEngine::Podman => {
                build_remote_podman_image().expect("Failed to build remote podman image")
            }
        };
        let nested_docker_dns = match engine {
            RemoteEngine::Docker => {
                let servers = host_dns_servers().expect("Failed to read host DNS servers");
                let servers = servers
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ");
                Some(format!("{NESTED_DOCKER_DNS_ENV}={servers}"))
            }
            RemoteEngine::Podman => None,
        };

        // Create temporary directory for SSH keys
        let temp_dir =
            TempDir::with_prefix("rumpelpod-ssh-test-").expect("Failed to create temp dir for SSH");
        let private_key_path = temp_dir.path().join("id_ed25519");
        let public_key_path = temp_dir.path().join("id_ed25519.pub");

        // Create a dedicated network to ensure IP stability
        let network_name = temp_dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        Command::new("docker")
            .args(["network", "create", &network_name])
            .success()
            .expect("Failed to create docker network");

        // Generate SSH key pair
        Command::new("ssh-keygen")
            .args(["-t", "ed25519"])
            .args(["-f", &private_key_path.to_string_lossy()])
            .args(["-N", ""]) // Empty passphrase
            .args(["-q"]) // Quiet
            .success()
            .expect("Failed to run ssh-keygen");

        // Read the public key
        let public_key =
            std::fs::read_to_string(&public_key_path).expect("Failed to read public key");

        // Start the container with privileged mode for the nested engine.
        // On macOS Docker Desktop, container IPs are inside the VM and not
        // routable from the host, so we publish the SSH port.
        let publish_ssh = force_publish_ssh || cfg!(target_os = "macos");
        let mut run_args = vec!["run", "-d", "--privileged", "--network"];
        run_args.push(&network_name);
        if engine == RemoteEngine::Podman {
            // Overlay upperdirs cannot live on the outer overlayfs and
            // this sandbox has no /dev/fuse for the fuse-overlayfs
            // fallback, so give the nested Podman tmpfs-backed storage.
            run_args.push("--tmpfs");
            run_args.push("/var/lib/containers:rw,size=4g,mode=0700");
        }
        if let Some(nested_docker_dns) = nested_docker_dns.as_deref() {
            run_args.push("--env");
            run_args.push(nested_docker_dns);
        }
        if publish_ssh {
            run_args.push("-p");
            run_args.push("0:22");
        }
        run_args.push(&image_id);

        let stdout = Command::new("docker")
            .args(&run_args)
            .success()
            .expect("Failed to start remote docker container");

        let container_id = String::from_utf8_lossy(&stdout).trim().to_string();

        // Get the container's IP address (used on Linux for direct access)
        let ip_address = get_container_ip(&container_id).expect("Failed to get container IP");

        // On macOS, find the published port for SSH
        let published_port = if publish_ssh {
            Some(get_published_port(&container_id, 22).expect("Failed to get published SSH port"))
        } else {
            None
        };

        let host = SshRemoteHost {
            container_id,
            engine,
            ip_address,
            published_port,
            _temp_dir: temp_dir,
            private_key_path,
            network_name,
        };

        // Install the public key for the test user
        host.install_public_key(&public_key);

        // Wait for SSH and Docker to be ready
        host.wait_for_services();

        host
    }

    /// Wait until we can actually execute a command over SSH.
    ///
    /// This is stronger than checking that sshd is listening: it verifies the
    /// full SSH handshake and command execution path works.
    fn wait_for_ssh_connectivity(&self, ssh_config: &Path) {
        let start = Instant::now();
        while start.elapsed() < SERVICE_TIMEOUT {
            let result = self.ssh_command(ssh_config, &["true"]);
            if result.is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!(
            "SSH connectivity not established within {:?}",
            SERVICE_TIMEOUT
        );
    }

    /// Get the SSH connection string for this remote host (ssh://user@host format).
    pub fn ssh_spec(&self) -> String {
        if let Some(port) = self.published_port {
            format!("ssh://{}@127.0.0.1:{}", SSH_USER, port)
        } else {
            format!("ssh://{}@{}", SSH_USER, self.ip_address)
        }
    }

    /// Get the SSH host identifier used in SSH config Host directives.
    pub fn ssh_host(&self) -> String {
        if self.published_port.is_some() {
            "127.0.0.1".to_string()
        } else {
            self.ip_address.clone()
        }
    }

    /// Get the path to the private key for this remote host.
    pub fn private_key_path(&self) -> &Path {
        &self.private_key_path
    }

    /// Install the SSH public key for the test user.
    fn install_public_key(&self, public_key: &str) {
        let setup_script = formatdoc! {r#"
            mkdir -p /home/{SSH_USER}/.ssh
            chmod 700 /home/{SSH_USER}/.ssh
            echo '{public_key}' >> /home/{SSH_USER}/.ssh/authorized_keys
            chmod 600 /home/{SSH_USER}/.ssh/authorized_keys
            chown -R {SSH_USER}:{SSH_USER} /home/{SSH_USER}/.ssh
        "#};

        Command::new("docker")
            .args(["exec", &self.container_id, "sh", "-c", &setup_script])
            .success()
            .expect("Failed to install SSH public key");
    }

    /// Wait for SSH and the container engine to be ready.
    fn wait_for_services(&self) {
        match self.engine {
            RemoteEngine::Docker => self.wait_for_docker(),
            RemoteEngine::Podman => self.wait_for_podman(),
        }
        self.wait_for_ssh();
    }

    fn wait_for_podman(&self) {
        let start = Instant::now();
        while start.elapsed() < SERVICE_TIMEOUT {
            // `podman version` requires a round-trip to the API socket,
            // unlike client-only commands, so it proves the service is up.
            let status = Command::new("docker")
                .args([
                    "exec",
                    "--env",
                    "CONTAINER_HOST=unix:///run/podman/podman.sock",
                    &self.container_id,
                    "podman",
                    "version",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            if matches!(status, Ok(s) if s.success()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!(
            "Podman did not become available within {:?}",
            SERVICE_TIMEOUT
        );
    }

    fn wait_for_docker(&self) {
        let start = Instant::now();
        while start.elapsed() < SERVICE_TIMEOUT {
            let status = Command::new("docker")
                .args(["exec", &self.container_id, "docker", "info"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            if matches!(status, Ok(s) if s.success()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!(
            "Docker did not become available within {:?}",
            SERVICE_TIMEOUT
        );
    }

    fn wait_for_ssh(&self) {
        let start = Instant::now();
        while start.elapsed() < SERVICE_TIMEOUT {
            let status = Command::new("docker")
                .args([
                    "exec",
                    &self.container_id,
                    "sh",
                    "-c",
                    "ss -tlnp | grep -q ':22'",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            if matches!(status, Ok(s) if s.success()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!("SSH did not become available within {:?}", SERVICE_TIMEOUT);
    }

    /// Run an SSH command on this remote host.
    pub fn ssh_command(&self, ssh_config: &Path, command: &[&str]) -> Result<Vec<u8>> {
        let config_path = ssh_config.to_string_lossy();
        let user_host = self.ssh_spec();

        let mut ssh_args = vec!["-F", &config_path, &user_host];
        ssh_args.extend(command.iter().copied());

        Command::new("ssh")
            .args(&ssh_args)
            .success()
            .context("running SSH command")
    }
}

impl Drop for SshRemoteHost {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container_id])
            .output();
        let _ = Command::new("docker")
            .args(["network", "rm", &self.network_name])
            .output();
    }
}

/// Get the host port that Docker published for a given container port.
fn get_published_port(container_id: &str, container_port: u16) -> Result<u16> {
    let stdout = Command::new("docker")
        .args([
            "inspect",
            "-f",
            &format!(
                "{{{{(index (index .NetworkSettings.Ports \"{}/tcp\") 0).HostPort}}}}",
                container_port
            ),
            container_id,
        ])
        .success()
        .context("getting published port")?;

    let port_str = String::from_utf8_lossy(&stdout).trim().to_string();
    port_str
        .parse()
        .with_context(|| format!("parsing published port: {:?}", port_str))
}

/// Get a container's IP address.
fn get_container_ip(container_id: &str) -> Result<String> {
    let stdout = Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            container_id,
        ])
        .success()
        .context("getting container IP")?;

    let ip = String::from_utf8_lossy(&stdout).trim().to_string();
    if ip.is_empty() {
        return Err(anyhow::anyhow!("container has no IP address"));
    }

    Ok(ip)
}

/// Return resolvers that remain reachable across another network namespace.
///
/// A Docker user-defined network writes only its embedded loopback resolver
/// into the outer container. Nested dockerd cannot reuse that address and
/// otherwise falls back to public resolvers that may be blocked.
fn host_dns_servers() -> Result<Vec<IpAddr>> {
    let resolv_conf = std::fs::read_to_string("/etc/resolv.conf")
        .context("reading /etc/resolv.conf for nested Docker DNS")?;
    parse_dns_servers(&resolv_conf)
}

fn parse_dns_servers(resolv_conf: &str) -> Result<Vec<IpAddr>> {
    let mut servers = Vec::new();
    for (index, line) in resolv_conf.lines().enumerate() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        let line_number = index + 1;
        let address = fields
            .next()
            .with_context(|| format!("nameserver on line {line_number} has no address"))?;
        let address = address
            .parse::<IpAddr>()
            .with_context(|| format!("invalid nameserver {address:?} on line {line_number}"))?;
        if !address.is_loopback() {
            servers.push(address);
        }
    }
    if servers.is_empty() {
        return Err(anyhow::anyhow!(
            "/etc/resolv.conf contains no non-loopback DNS servers"
        ));
    }
    Ok(servers)
}

/// Build the Docker image for the remote Docker host test container.
///
/// This is infrastructure (not a test pod image), so it builds directly
/// via `docker build` rather than going through the devcontainer path.
fn build_remote_docker_image() -> Result<String> {
    let dockerfile = formatdoc! {r#"
        FROM debian:13

        # Install SSH server, Docker, and utilities
        RUN apt-get update && apt-get install -y \
            openssh-server \
            docker.io \
            git \
            iproute2 \
            && rm -rf /var/lib/apt/lists/*

        # Create test user with Docker access
        RUN useradd -m -u {TEST_USER_UID} -s /bin/bash {SSH_USER} \
            && usermod -aG docker {SSH_USER}

        # Configure SSH to mimic a Teleport-style locked-down server:
        # AllowStreamLocalForwarding=no rejects `ssh -L <sock>:/remote.sock`
        # (the `direct-streamlocal@openssh.com` channel type) with
        # "administratively prohibited".  AllowTcpForwarding stays at
        # its default (yes) because devcontainer `forwardPorts` uses
        # `direct-tcpip`, which Teleport also allows.
        # MaxSessions is raised because one ControlMaster carries every
        # pod route (dial-stdio, git tunnel, exec proxy).
        RUN mkdir -p /run/sshd \
            && ssh-keygen -A \
            && sed -i 's/#PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config \
            && sed -i 's/#PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config \
            && sed -i 's/#PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config \
            && echo 'AllowStreamLocalForwarding no' >> /etc/ssh/sshd_config \
            && echo 'MaxSessions 50' >> /etc/ssh/sshd_config \
            && echo 'MaxStartups 50:30:100' >> /etc/ssh/sshd_config

        # Startup script that runs both SSH and Docker.
        # After a container restart, stale PID and socket files from the previous
        # dockerd/containerd can prevent startup (dockerd sees the old containerd PID
        # file, thinks it is "still running", and hangs). We clean those up first.
        RUN echo '#!/bin/bash\n\
            set -e\n\
            rm -f /var/run/docker.pid\n\
            rm -f /var/run/docker/containerd/containerd.pid\n\
            rm -f /var/run/docker/containerd/containerd.sock*\n\
            dns_args=()\n\
            for dns_server in ${NESTED_DOCKER_DNS_ENV}; do\n\
                dns_args+=(--dns "$dns_server")\n\
            done\n\
            dockerd "${{dns_args[@]}}" &\n\
            for i in $(seq 1 60); do\n\
                if docker info >/dev/null 2>&1; then break; fi\n\
                sleep 1\n\
            done\n\
            exec /usr/sbin/sshd -D\n\
        ' > /start.sh && chmod +x /start.sh

        CMD ["/start.sh"]
    "#};

    let temp_dir = TempDir::with_prefix("rumpelpod-ssh-image-build-")
        .context("creating temp dir for SSH image build")?;
    std::fs::write(temp_dir.path().join("Dockerfile"), &dockerfile)
        .context("writing Dockerfile")?;

    let output = Command::new("docker")
        .args(["build", "-q", temp_dir.path().to_str().unwrap()])
        .output()
        .context("executing docker build for SSH server image")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("docker build failed: {stderr}"));
    }

    let image_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if image_id.is_empty() {
        return Err(anyhow::anyhow!("docker build returned empty image ID"));
    }

    Ok(image_id)
}

/// Build the Docker image for the remote Podman host test container.
///
/// The container runs a rootful `podman system service` because rootless
/// Podman cannot set up its user namespace in this sandbox (newuidmap is
/// blocked even in privileged containers).  The SSH user reaches the
/// rootful socket through group membership, and sshd's `SetEnv` points
/// the user's podman client (including `podman system dial-stdio`) at it.
/// On a real rootless remote, dial-stdio resolves the user's own socket
/// instead and no such setup is needed.
fn build_remote_podman_image() -> Result<String> {
    let dockerfile = formatdoc! {r#"
        FROM debian:13

        # Install SSH server, Podman, and utilities
        RUN apt-get update && apt-get install -y \
            openssh-server \
            podman \
            git \
            iproute2 \
            && rm -rf /var/lib/apt/lists/*

        # Create test user with access to the rootful Podman socket
        RUN useradd -m -u {TEST_USER_UID} -s /bin/bash {SSH_USER} \
            && groupadd podman-sock \
            && usermod -aG podman-sock {SSH_USER}

        # Native overlay on the tmpfs mounted at /var/lib/containers by
        # the fixture; the Debian default would probe for fuse-overlayfs.
        # Host networking because container network setup (pasta or a
        # netavark bridge) needs /dev/net/tun, which this nested
        # container lacks; same approach as the rumpelpod devcontainer.
        RUN printf '%s\n' \
            '[storage]' \
            'driver = "overlay"' \
            'runroot = "/run/containers/storage"' \
            'graphroot = "/var/lib/containers/storage"' \
            > /etc/containers/storage.conf \
            && printf '%s\n' \
            '[containers]' \
            'netns = "host"' \
            > /etc/containers/containers.conf

        # Same Teleport-style locked-down sshd as the remote Docker
        # image: no streamlocal forwarding, so the transport must use
        # the exec channel (`podman system dial-stdio`), not Podman's
        # built-in ssh:// socket forwarding.
        RUN mkdir -p /run/sshd \
            && ssh-keygen -A \
            && sed -i 's/#PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config \
            && sed -i 's/#PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config \
            && sed -i 's/#PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config \
            && echo 'AllowStreamLocalForwarding no' >> /etc/ssh/sshd_config \
            && echo 'MaxSessions 50' >> /etc/ssh/sshd_config \
            && echo 'MaxStartups 50:30:100' >> /etc/ssh/sshd_config \
            && echo 'SetEnv CONTAINER_HOST=unix:///run/podman/podman.sock' >> /etc/ssh/sshd_config

        # Startup script that runs both the Podman API service and SSH.
        RUN echo '#!/bin/bash\n\
            set -e\n\
            mkdir -p /run/podman\n\
            rm -f /run/podman/podman.sock\n\
            podman system service --time=0 unix:///run/podman/podman.sock &\n\
            for i in $(seq 1 60); do\n\
                if [ -S /run/podman/podman.sock ]; then break; fi\n\
                sleep 1\n\
            done\n\
            chgrp podman-sock /run/podman/podman.sock\n\
            chmod 660 /run/podman/podman.sock\n\
            exec /usr/sbin/sshd -D\n\
        ' > /start.sh && chmod +x /start.sh

        CMD ["/start.sh"]
    "#};

    let temp_dir = TempDir::with_prefix("rumpelpod-podman-ssh-image-build-")
        .context("creating temp dir for Podman SSH image build")?;
    std::fs::write(temp_dir.path().join("Dockerfile"), &dockerfile)
        .context("writing Dockerfile")?;

    let output = Command::new("docker")
        .args(["build", "-q", temp_dir.path().to_str().unwrap()])
        .output()
        .context("executing docker build for Podman SSH server image")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("docker build failed: {stderr}"));
    }

    let image_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if image_id.is_empty() {
        return Err(anyhow::anyhow!("docker build returned empty image ID"));
    }

    Ok(image_id)
}

/// Write an SSH config into `home/.ssh/` for the given remote hosts.
///
/// The config provides isolation from the user's SSH configuration:
/// - Uses a separate known_hosts file inside the test home
/// - Configures each host with its specific identity file
/// - Disables the ambient SSH agent and restricts authentication to those
///   identities
///
/// Also links `ssh` and `docker` into the test home's bin dir: the
/// daemon needs `ssh` to run remote docker dial-stdio and `docker`
/// to drive Docker CLI operations against the SSH host.  Tests that reach for
/// `SshRemoteHost` directly (bypassing `ExecutorResources::ssh`)
/// still get the right PATH narrowing through this helper.
pub fn write_ssh_config(home: &TestHome, hosts: &[&SshRemoteHost]) {
    home.link_local_bins(&["ssh", "docker"]);
    let ssh_dir = home.path().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).expect("Failed to create .ssh directory");

    let config_path = ssh_dir.join("config");
    let known_hosts_path = ssh_dir.join("known_hosts");

    let mut config = formatdoc! {r#"
        Host *
            UserKnownHostsFile {known_hosts}
            StrictHostKeyChecking accept-new
            BatchMode yes
            ConnectTimeout 10
            IdentityAgent none

    "#,
        known_hosts = known_hosts_path.display(),
    };

    for host in hosts {
        let mut host_config = formatdoc! {r#"
            Host {ssh_host}
                IdentityFile {key}
                IdentitiesOnly yes
        "#,
            ssh_host = host.ssh_host(),
            key = host.private_key_path().display(),
        };
        if let Some(port) = host.published_port {
            host_config.push_str(&format!("    Port {}\n", port));
        }
        host_config.push('\n');
        config.push_str(&host_config);
    }

    std::fs::write(&config_path, config).expect("Failed to write SSH config");
    install_ssh_config_wrapper(home, &config_path);
}

/// Write an SSH config that exposes a remote host only through an alias.
///
/// Rumpelpod should pass the alias to OpenSSH without forcing user,
/// hostname, or port settings on the command line.
pub fn write_ssh_alias_config(home: &TestHome, host: &SshRemoteHost, alias: &str) {
    home.link_local_bins(&["ssh", "docker"]);
    let ssh_dir = home.path().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).expect("Failed to create .ssh directory");

    let config_path = ssh_dir.join("config");
    let known_hosts_path = ssh_dir.join("known_hosts");

    let mut config = formatdoc! {r#"
        Host *
            UserKnownHostsFile {known_hosts}
            StrictHostKeyChecking accept-new
            BatchMode yes
            ConnectTimeout 10
            IdentityAgent none

        Host {alias}
            HostName {ssh_host}
            User {SSH_USER}
            IdentityFile {key}
            IdentitiesOnly yes
    "#,
        known_hosts = known_hosts_path.display(),
        ssh_host = host.ssh_host(),
        key = host.private_key_path().display(),
    };

    if let Some(port) = host.published_port {
        config.push_str(&format!("    Port {port}\n"));
    }

    std::fs::write(&config_path, config).expect("Failed to write SSH alias config");
    install_ssh_config_wrapper(home, &config_path);
}

fn install_ssh_config_wrapper(home: &TestHome, config_path: &Path) {
    // The SSH client resolves config from the passwd home, not the
    // HOME env var TestHome changes, so keep fixture config explicit.
    let wrapper_path = home.bin_dir().join("ssh");
    let real_ssh = match std::fs::read_link(&wrapper_path) {
        Ok(path) => path,
        Err(_) => return,
    };

    let real_ssh = shell_quote(&real_ssh.to_string_lossy());
    let config_path = shell_quote(&config_path.to_string_lossy());
    let script = formatdoc! {r#"
        #!/bin/sh
        exec {real_ssh} -F {config_path} "$@"
    "#};

    std::fs::remove_file(&wrapper_path).expect("removing ssh symlink");
    std::fs::write(&wrapper_path, script).expect("writing ssh config wrapper");
    let mut perms = std::fs::metadata(&wrapper_path)
        .expect("reading ssh wrapper metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper_path, perms).expect("marking ssh wrapper executable");
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

mod reconnect;

// Tests
// Note: These tests require privileged Docker containers, which may not be
// available in all CI environments.

#[test]
fn dns_servers_exclude_loopback_resolvers() {
    let servers = parse_dns_servers(indoc! {r#"
        nameserver 127.0.0.11
        nameserver 172.16.200.26
        nameserver ::1
        nameserver 2001:db8::53
        search example.test
    "#})
    .unwrap();

    assert_eq!(
        servers,
        vec![
            "172.16.200.26".parse::<IpAddr>().unwrap(),
            "2001:db8::53".parse::<IpAddr>().unwrap(),
        ]
    );
}

#[test]
fn dns_servers_require_a_routable_resolver() {
    let error = parse_dns_servers(indoc! {r#"
        nameserver 127.0.0.11
        nameserver ::1
    "#})
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "/etc/resolv.conf contains no non-loopback DNS servers"
    );
}

#[test]
fn ssh_smoke_test() {
    // This file exercises the SSH remote-Docker executor.  Under the
    // k8s executor there is no SSH path in play and the test would
    // simply duplicate the k8s smoke coverage.
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker | crate::executor::ExecutorMode::Ssh
    ) {
        crate::executor::skip_test();
        return;
    }
    let home = TestHome::new();
    let executor = ExecutorResources::setup(&home);
    let daemon = TestDaemon::start(&home);
    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    std::fs::write(repo.path().join(".rumpelpod.json"), &executor.json).unwrap();

    // Enter the pod on the remote Docker host
    let pod_name = "remote-test";
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "echo",
            "hello from remote",
        ])
        .output()
        .expect("rumpel enter failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "rumpel enter failed: stdout={}, stderr={}",
        stdout,
        stderr
    );
    assert_eq!(stdout.trim(), "hello from remote");
}

#[test]
fn ssh_uses_user_config_for_omitted_port() {
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker | crate::executor::ExecutorMode::Ssh
    ) {
        crate::executor::skip_test();
        return;
    }

    let home = TestHome::new();
    let remote = SshRemoteHost::start_published();
    let alias = "rumpelpod-ssh-alias";
    write_ssh_alias_config(&home, &remote, alias);
    let daemon = TestDaemon::start(&home);

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let remote_spec = format!("ssh://{alias}");
    let config = serde_json::to_string(&serde_json::json!({"host": remote_spec})).unwrap();
    std::fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", "alias-test", "--", "true"])
        .output()
        .expect("rumpel enter failed to execute");

    assert!(
        output.status.success(),
        "rumpel enter via SSH alias failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Skipped on macOS because the nested SSH host fixture is not reliable
/// through Colima's VM networking layer.
#[test]
fn ssh_reconnect_test() {
    if cfg!(target_os = "macos") {
        crate::executor::skip_test();
        return;
    }
    // Hardcodes a Docker-backed SshRemoteHost, so only runs where the
    // test process has a reachable local Docker daemon.
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker | crate::executor::ExecutorMode::Ssh
    ) {
        crate::executor::skip_test();
        return;
    }
    // This test needs direct access to the SshRemoteHost to kill existing transports,
    // so it sets up SSH infrastructure manually rather than via ExecutorResources.
    let home = TestHome::new();
    let remote = SshRemoteHost::start();
    write_ssh_config(&home, &[&remote]);
    let mut daemon = TestDaemon::start(&home);

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let remote_spec = remote.ssh_spec();
    let config = serde_json::to_string(&serde_json::json!({"host": remote_spec})).unwrap();
    std::fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    // Enter the pod on the remote Docker host
    let pod_name = "reconnect-test";
    let output = pod_command(&repo, &daemon)
        .args([
            "enter",
            "--create",
            pod_name,
            "--",
            "echo",
            "hello from remote",
        ])
        .output()
        .expect("rumpel enter failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "first rumpel enter failed: stdout={}, stderr={}",
        stdout,
        stderr
    );
    assert_eq!(stdout.trim(), "hello from remote");

    daemon.kill();
    let daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["connect", pod_name])
        .success()
        .expect("establishing missing pod routes after daemon startup failed");

    let ssh_config = home.path().join(".ssh/config");
    let inner_container = remote
        .ssh_command(
            &ssh_config,
            &[
                "docker",
                "ps",
                "-q",
                "--filter",
                &format!("label=dev.rumpelpod.name={pod_name}"),
            ],
        )
        .expect("finding remote pod failed");
    let inner_container = String::from_utf8_lossy(&inner_container).trim().to_string();
    assert!(!inner_container.is_empty(), "remote pod not found");
    remote
        .ssh_command(
            &ssh_config,
            &[
                "docker",
                "exec",
                &inner_container,
                "git",
                "-c",
                "core.hooksPath=/dev/null",
                "-C",
                TEST_REPO_PATH,
                "commit",
                "--allow-empty",
                "-m",
                "pending-host-recovery",
            ],
        )
        .expect("creating pending remote commit failed");
    let pending_commit = remote
        .ssh_command(
            &ssh_config,
            &[
                "docker",
                "exec",
                &inner_container,
                "git",
                "-C",
                TEST_REPO_PATH,
                "rev-parse",
                "HEAD",
            ],
        )
        .expect("reading pending remote commit failed");
    let pending_commit = String::from_utf8_lossy(&pending_commit).trim().to_string();
    let expected_ref = format!("refs/rumpelpod/{pod_name}@{pod_name}");

    // Kill established sshd children but leave the listener, nested Docker,
    // and pod running. New connections remain available while every daemon
    // transport, including the git tunnel, has to be replaced.
    Command::new("docker")
        .args([
            "exec",
            &remote.container_id,
            "sh",
            "-c",
            "for pid in $(pgrep sshd); do [ \"$pid\" = 1 ] || kill -9 \"$pid\"; done",
        ])
        .success()
        .expect("terminating existing SSH transports failed");
    remote.wait_for_ssh_connectivity(&ssh_config);

    pod_command(&repo, &daemon)
        .args(["connect", pod_name])
        .success()
        .expect("connecting through the restored SSH host failed");

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let output = Command::new("git")
            .args(["rev-parse", &expected_ref])
            .current_dir(repo.path())
            .output()
            .expect("git rev-parse failed");
        if output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim() == pending_commit
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "connect did not recover the git tunnel"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // Try to enter again
    let output = pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "echo", "hello again"])
        .output()
        .expect("rumpel enter failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "second rumpel enter failed: stdout={}, stderr={}",
        stdout,
        stderr
    );
    assert_eq!(stdout.trim(), "hello again");
}

/// All pod routes sharing an SSH host must recover after the host disconnects.
/// A single pod does not reproduce the stalled route seen after laptop resume.
#[test]
fn multiple_pods_reconnect_after_ssh_transport_stall() {
    if cfg!(target_os = "macos") {
        crate::executor::skip_test();
        return;
    }
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker | crate::executor::ExecutorMode::Ssh
    ) {
        crate::executor::skip_test();
        return;
    }

    let home = TestHome::new();
    let remote = SshRemoteHost::start();
    write_ssh_config(&home, &[&remote]);
    let ssh_config = home.path().join(".ssh/config");
    let mut ssh_config_content = std::fs::read_to_string(&ssh_config).unwrap();
    let control_path = home.path().join("ssh-control-%C");
    ssh_config_content.push_str(&formatdoc! {r#"
        Host *
            ControlMaster auto
            ControlPath {control_path}
            ControlPersist 10m
            ServerAliveInterval 1
            ServerAliveCountMax 2
            TCPKeepAlive yes

    "#, control_path = control_path.display()});
    std::fs::write(&ssh_config, ssh_config_content).unwrap();
    let daemon = TestDaemon::start(&home);

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let config = serde_json::to_string(&serde_json::json!({"host": remote.ssh_spec()})).unwrap();
    std::fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    let pod_names = ["shared-a", "shared-b", "shared-c", "shared-d", "foo-bar"];
    for pod_name in pod_names {
        pod_command(&repo, &daemon)
            .args(["enter", "--create", pod_name, "--", "true"])
            .success()
            .unwrap_or_else(|e| panic!("creating pod {pod_name} failed: {e:#}"));
    }

    reconnect::wait_for_mux(
        &home,
        &remote,
        reconnect::MuxState::Alive,
        "mux after setup",
    );
    Command::new("docker")
        .args(["pause", &remote.container_id])
        .success()
        .expect("pausing the SSH host failed");
    reconnect::wait_for_mux(
        &home,
        &remote,
        reconnect::MuxState::Dead,
        "mux while paused",
    );

    Command::new("docker")
        .args(["unpause", &remote.container_id])
        .success()
        .expect("resuming the SSH host failed");
    remote.wait_for_ssh_connectivity(&ssh_config);

    let stdout = pod_command(&repo, &daemon)
        .args(["enter", "foo-bar", "--", "echo", "reconnected"])
        .success()
        .expect("entering a pod after SSH resumed failed");
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "reconnected");
}

#[test]
fn ssh_unavailable_reentry_preserves_pod_record() {
    if !matches!(
        crate::executor::executor_mode(),
        crate::executor::ExecutorMode::Docker
    ) {
        crate::executor::skip_test();
        return;
    }

    let home = TestHome::new();
    let remote = SshRemoteHost::start();
    write_ssh_config(&home, &[&remote]);

    let repo = TestRepo::new();
    write_test_devcontainer(&repo, "", "");
    let remote_spec = remote.ssh_spec();
    let config = serde_json::to_string(&serde_json::json!({"host": remote_spec})).unwrap();
    std::fs::write(repo.path().join(".rumpelpod.json"), config).unwrap();

    let pod_name = "ssh-down";
    let mut daemon = TestDaemon::start(&home);
    pod_command(&repo, &daemon)
        .args(["enter", "--create", pod_name, "--", "true"])
        .success()
        .expect("rumpel enter failed");

    daemon.kill();
    drop(daemon);

    Command::new("docker")
        .args(["stop", &remote.container_id])
        .success()
        .expect("failed to stop remote host container");

    let daemon = TestDaemon::start(&home);
    let output = pod_command(&repo, &daemon)
        .args(["enter", pod_name, "--", "true"])
        .output()
        .expect("rumpel enter failed to execute");

    assert!(
        !output.status.success(),
        "enter should fail while the ssh connection is unavailable"
    );

    let stdout = pod_command(&repo, &daemon)
        .arg("list")
        .success()
        .expect("rumpel list failed");
    let list_output = String::from_utf8_lossy(&stdout);
    assert!(
        list_output.contains(pod_name),
        "list should still show pod after reconnect failure: {list_output}",
    );
    assert!(
        list_output.contains("disconnected"),
        "list should report disconnected status after reconnect failure: {list_output}",
    );
}
