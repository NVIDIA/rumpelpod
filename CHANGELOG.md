# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - TBD

### Added

- Initial public release of Rumpelpod, an isolated runner for LLM coding
  agents.
- `rumpel` CLI for managing named pods (workspaces) of a git repository
  inside Docker containers or Kubernetes pods.
- Local Docker, remote Docker via SSH, and Kubernetes pod backends.
- First-class integration with Anthropic's Claude Code (`rumpel claude`)
  and OpenAI's Codex (`rumpel codex`).
- Git push/pull synchronisation between the host workspace and the pod.
- Port forwarding and container lifecycle management.
- `devcontainer.json`-based image configuration.
- Compatibility with hardened container runtimes such as gVisor and
  Kata Containers.
