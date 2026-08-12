# Rumpelpod for VS Code

This workspace extension adds a Rumpelpod mode to the VS Code Activity Bar.
The mode renders agent sessions and a pod shell in xterm.js terminals shipped
by the extension and keeps them in the primary sidebar. The pod's review is a
native VS Code multi-diff in the main editor area. VS Code and the user's
`diffEditor.renderSideBySide` preference decide whether that diff is inline or
side by side.

Click the Rumpelpod Activity Bar icon to enter the mode. Click the pod name at
the top of the view to switch pods. The `+` in the native view title creates a
pod, separate from the controls that operate on the selected pod. The icons in
the pod header open a shell, launch another agent, and merge the pod. Its
overflow menu groups review actions, SSH key access, and the stop and delete
lifecycle actions. Adding an SSH key opens VS Code's extension-host file picker;
encrypted keys prompt for their passphrase without storing it.
Selectors and pod creation stay beside their controls in sidebar popovers.
A single session uses the whole sidebar; named agent and shell tabs appear only
while two or more sessions are open. Launching Claude while Codex is open keeps
both independent PTYs alive instead of replacing one with the other. Selecting
a pod restores its launched agents and focuses every changed file on the right
in one vertically stacked review. The review remains an ordinary closable VS
Code editor. Selecting the pod again or choosing `View diff` from the actions
menu focuses the current review, reopening it through VS Code's public changes
editor command when necessary. Background daemon events refresh the active
review without stealing focus from another editor or resurrecting a closed one.
Switching pods focuses that pod's review, or opens one if needed. The extension
does not pin, unpin, retain, or reposition review tabs, and does not change VS
Code's editor-tab settings. Tab placement and preview replacement therefore
follow the user's editor preferences, including
`workbench.editor.openPositioning`. Pods without changed files still open a
native empty review surface instead of leaving the previous editor visible. VS
Code restores ordinary review editors across browser reloads; the extension
restores the last active pod and its launched agent set, then reopens that pod's
current review.

Forwarded ports are available from the pod actions menu and the Command
Palette. `Open port in VS Code...` uses a restricted webview tab in the active
editor group and prefixes the tab with the pod name. VS Code and the user's
editor settings decide the tab position. The nested page has an opaque origin
and cannot navigate the VS Code tab, open popups, download files, or access VS
Code and workspace resources. Sites that need cookies, browser storage, or
those blocked features can instead use `Open port in browser...`, which is
always an explicit action.
A Dev Container port marked with
`"onAutoForward": "openPreview"` opens automatically in the active editor
group without taking focus; `"protocol": "https"` selects HTTPS, otherwise
HTTP is used.
The `openBrowser` and `openBrowserOnce` actions do not launch an external
browser automatically. Each forwarded port gets its own preview tab.

Merging, stopping, or deleting the selected pod clears the sidebar selection,
detaches its sessions, forgets it as the last active pod, and closes the review
tracked by the extension.

Pod lists, review plans, and live invalidations come directly from the local
rumpelpod daemon over its Unix socket. The configured `rumpel` executable is
used for lifecycle actions and interactive terminals, not as a JSON transport.

## Development

The Rust type generator owns `src/generated/`. The repository tool regenerates
those bindings, packages the extension, rebuilds the development daemon,
installs both, and restarts the browser workspace:

```sh
cargo vscode
```

The devcontainer image installs the extension and browser service but no daemon
unit. A lingering systemd user session starts the browser service during
container boot. The first `cargo pipeline` installs and starts the freshly built
daemon and its cross-architecture pod payloads; the browser reconnects when the
daemon socket appears. It serves a dedicated `anyhow` 1.0.102 demo repository
from `/workspaces/anyhow-demo` at port 3000 without depending on devcontainer
lifecycle commands. This keeps the live editor outside the rumpelpod source
checkout: pods created through the extension use the demo's ordinary Rust
devcontainer instead of nesting another copy of this development container. The
demo is seeded from Cargo's pinned registry source during the image build; later
`cargo vscode` runs preserve its pods and working tree.

The server is unauthenticated and its address is fixed to the container
loopback interface, so it cannot be exposed on a container network interface.
Use the forwarded URL rather than trying to reach the container directly. When
this repository is itself inside a rumpelpod, run `rumpel ports POD_NAME` in
the parent checkout and open the local port labeled `Rumpelpod VS Code`;
rumpel also binds that forward only on the host loopback interface.

Run `npm ci` once and then use `npm run watch` for fast TypeScript compilation
feedback. Run `cargo vscode` to put any change into the live browser workspace;
it refreshes the daemon, generated contracts, installed VSIX, and user service
files, then waits until the browser service is healthy. `cargo vscode --check`
performs the extension checks used by the Rust pipeline without updating either
live service.

The embedded terminal uses a native PTY binding. `npm run package` labels the
VSIX for the current operating system and architecture and includes the binding
built on that host. Tagged releases provide Linux x64, Linux arm64, and macOS
arm64 bundles as separate assets. CI builds Linux release bundles in a pinned
Bullseye container and rejects native bindings that require a newer glibc than
the supported baseline.

The browser integration test launches an isolated code-server instance and a
real rumpelpod daemon, then drives the packaged extension with Playwright. It
is ignored by default because it is slow and exercises timing-sensitive browser
and session restoration. Run it explicitly through the normal test pipeline:

```sh
cargo pipeline vscode_browser_lists_creates_and_reviews_pods -- --ignored
```

Successful runs capture the Activity Bar mode, pod switcher, agent launcher,
pod-name prompt, concurrent Claude and Codex terminals, the embedded shell,
terminal-plus-review layout, restored sessions, and a Playwright trace under
`target/vscode-integration/`. To refresh the checked-in reference images while
running the same test, use:

```sh
RUMPELPOD_VSCODE_REFERENCE_IMAGES="$PWD/vscode/docs/images" \
    cargo pipeline vscode_browser_lists_creates_and_reviews_pods -- --ignored
```

The creation flow reaches a real Codex prompt while the daemon reports the new
pod as running:

![Created pod with a live embedded Codex terminal](docs/images/06-created-chat.png)

The shell tab uses a second extension-owned xterm and keeps the agent attached:

![Embedded pod shell beside the native review](docs/images/09-shell.png)

The test then makes a real commit inside the active pod. The daemon event
updates both the status item and the review without a manual refresh:

![Sidebar agent terminal and live review diff](docs/images/02-live-review.png)
