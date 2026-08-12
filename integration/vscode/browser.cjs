// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { createRequire } = require("node:module");

const requireFromExtension = createRequire(path.join(process.cwd(), "package.json"));
const { chromium } = requireFromExtension("@playwright/test");

const ADDED_FILE = "browser-added.txt";
const ADDED_CONTENT = "content added in the pod";

const REFERENCE_IMAGES = [
    "01-default-chat.png",
    "02-live-review.png",
    "03-pod-switcher.png",
    "04-agent-picker.png",
    "05-pod-name.png",
    "06-created-chat.png",
    "07-switched-review.png",
    "08-restored-chat.png",
    "09-shell.png",
];

function requiredEnvironment(name) {
    const value = process.env[name];
    assert(value, `${name} is required`);
    return value;
}

async function waitForAction(actionLog, expected, occurrences = 1) {
    const deadline = Date.now() + 30_000;
    let content = "";
    while (Date.now() < deadline) {
        if (fs.existsSync(actionLog)) {
            content = fs.readFileSync(actionLog, "utf8");
            if (content.split("\n").filter((line) => line === expected).length >= occurrences) {
                return;
            }
        }
        await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(`action log omitted ${JSON.stringify(expected)}: ${JSON.stringify(content)}`);
}

async function capture(page, artifacts, name) {
    fs.mkdirSync(artifacts, { recursive: true });
    const artifactPath = path.join(artifacts, name);
    await page.screenshot({
        path: artifactPath,
        fullPage: true,
    });
}

function publishReferenceImages(artifacts, referenceImages) {
    if (!referenceImages) {
        return;
    }
    for (const name of REFERENCE_IMAGES) {
        const artifact = path.join(artifacts, name);
        assert(fs.existsSync(artifact), `successful browser run omitted ${artifact}`);
    }
    fs.mkdirSync(referenceImages, { recursive: true });
    for (const name of REFERENCE_IMAGES) {
        fs.copyFileSync(path.join(artifacts, name), path.join(referenceImages, name));
    }
}

function statusItem(page, text) {
    return page.locator(".statusbar-item").filter({ hasText: text }).first();
}

async function waitForStatusItem(page, text) {
    const status = statusItem(page, text);
    await status.waitFor({ state: "visible", timeout: 30_000 });
    return status;
}

async function waitForPersistedPodStatus(page, podName) {
    const status = await waitForStatusItem(page, podName);
    const deadline = Date.now() + 90_000;
    let rendered = "";
    while (Date.now() < deadline) {
        rendered = (await status.textContent()) || "";
        if (!rendered.includes("/ starting")) {
            return;
        }
        await page.waitForTimeout(200);
    }
    throw new Error(`pod ${podName} remained an optimistic starting status: ${rendered}`);
}

async function openRumpelpodView(page) {
    const action = page.locator('.activitybar a[aria-label="Rumpelpod"]');
    await action.waitFor({ state: "visible", timeout: 30_000 });
    await action.click();
    const sidebar = page.locator(".part.sidebar:visible");
    await sidebar.waitFor({ state: "visible", timeout: 30_000 });
    await sidebar
        .locator(".composite.title")
        .filter({ hasText: "Rumpelpod" })
        .waitFor({ state: "visible", timeout: 30_000 });
    return sidebar;
}

async function openPodSwitcher(page) {
    const frame = await waitForAgentFrame(page);
    const podSelector = frame.locator("#pod-selector");
    await podSelector.waitFor({ state: "visible", timeout: 30_000 });
    await podSelector.click();
    const popover = frame.locator("#pod-popover");
    await popover.waitFor({ state: "visible", timeout: 30_000 });
    await assertPopoverAnchored(podSelector, popover);
    await assertPodSwitcherHasNoFilter(popover);
    assert.equal(
        await page.locator(".quick-input-widget:visible").count(),
        0,
        "pod selection opened VS Code Quick Access",
    );
    return { frame, popover };
}

async function waitForInitialPodSwitcher(page) {
    const frame = await waitForAgentFrame(page);
    const podSelector = frame.locator("#pod-selector");
    const popover = frame.locator("#pod-popover");
    await popover.waitFor({ state: "visible", timeout: 30_000 });
    await assertPopoverAnchored(podSelector, popover);
    await assertPodSwitcherHasNoFilter(popover);
    assert.equal(
        await page.locator(".quick-input-widget:visible").count(),
        0,
        "initial pod selection opened VS Code Quick Access",
    );
    return { frame, popover };
}

async function assertPodSwitcherHasNoFilter(popover) {
    assert.equal(
        await popover.locator('input[aria-label="Filter pods"]').count(),
        0,
        "pod selection retained an unnecessary filter",
    );
    assert(
        !((await popover.textContent()) ?? "").includes("Filter pods"),
        "pod selection rendered filter text",
    );
}

async function togglePopover(trigger, popover) {
    await trigger.click();
    await popover.waitFor({ state: "hidden", timeout: 30_000 });
    assert.equal(
        await trigger.getAttribute("aria-expanded"),
        "false",
        "closing a popover did not collapse its trigger",
    );
    await trigger.click();
    await popover.waitFor({ state: "visible", timeout: 30_000 });
    assert.equal(
        await trigger.getAttribute("aria-expanded"),
        "true",
        "reopening a popover did not expand its trigger",
    );
    await assertPopoverAnchored(trigger, popover);
}

async function toggleNativeCreatePopover(trigger, popover) {
    await trigger.click();
    await popover.waitFor({ state: "hidden", timeout: 30_000 });
    await trigger.click();
    await popover.waitFor({ state: "visible", timeout: 30_000 });
    await assertNativePopoverAnchored(trigger, popover);
}

function popoverRows(popover) {
    return popover.locator('[role="option"], [role="menuitem"]');
}

async function selectPopoverOption(popover, label) {
    const row = popoverRows(popover).filter({ hasText: label }).first();
    await row.waitFor({ state: "visible", timeout: 30_000 });
    await row.click();
    await popover.waitFor({ state: "hidden", timeout: 30_000 });
}

async function assertPopoverAnchored(trigger, popover) {
    const triggerBounds = await trigger.boundingBox();
    const popoverBounds = await popover.boundingBox();
    const viewportWidth = await popover.evaluate(() => window.innerWidth);
    assert(triggerBounds, "popover trigger had no bounds");
    assert(popoverBounds, "popover had no bounds");
    assert(
        Math.max(triggerBounds.x, popoverBounds.x) <=
            Math.min(triggerBounds.x + triggerBounds.width, popoverBounds.x + popoverBounds.width),
        `popover was not horizontally aligned with its trigger: ${JSON.stringify({ triggerBounds, popoverBounds })}`,
    );
    assert(
        popoverBounds.y >= triggerBounds.y + triggerBounds.height - 1 &&
            popoverBounds.y - (triggerBounds.y + triggerBounds.height) <= 8,
        `popover did not open below its trigger: ${JSON.stringify({ triggerBounds, popoverBounds })}`,
    );
    assert(
        popoverBounds.width <= viewportWidth - 8,
        `popover escaped the sidebar webview: ${JSON.stringify({ viewportWidth, popoverBounds })}`,
    );
}

async function assertNativePopoverAnchored(trigger, popover) {
    const triggerBounds = await trigger.boundingBox();
    const popoverBounds = await popover.boundingBox();
    assert(triggerBounds, "native create action had no bounds");
    assert(popoverBounds, "create popover had no bounds");
    assert(
        Math.max(triggerBounds.x, popoverBounds.x) <=
            Math.min(triggerBounds.x + triggerBounds.width, popoverBounds.x + popoverBounds.width),
        `create popover was not aligned with the native action: ${JSON.stringify({ triggerBounds, popoverBounds })}`,
    );
    assert(
        popoverBounds.y >= triggerBounds.y + triggerBounds.height - 1 &&
            popoverBounds.y - (triggerBounds.y + triggerBounds.height) <= 24,
        `create popover did not open below the native action: ${JSON.stringify({ triggerBounds, popoverBounds })}`,
    );
}

async function confirmModal(page, message, action) {
    const dialog = page.locator(".monaco-dialog-box:visible").filter({ hasText: message });
    await dialog.waitFor({ state: "visible", timeout: 30_000 });
    const button = dialog.getByRole("button", { name: action, exact: true });
    await button.waitFor({ state: "visible", timeout: 30_000 });
    await button.click();
    await dialog.waitFor({ state: "hidden", timeout: 30_000 });
}

function quickPickRows(page) {
    return page.locator(".quick-input-widget:visible .quick-input-list .monaco-list-row");
}

async function quickPickRow(page, label) {
    const rows = quickPickRows(page);
    await rows.first().waitFor({ state: "visible", timeout: 30_000 });
    const row = rows.filter({ hasText: label });
    await row.first().waitFor({ state: "visible", timeout: 30_000 });
    return row.first();
}

async function selectQuickPick(page, label) {
    const row = await quickPickRow(page, label);
    await row.click();
    if (await row.isVisible()) {
        await page.keyboard.press("Enter");
    }
    await row.waitFor({ state: "hidden", timeout: 30_000 });
}

async function waitForQuickPickLabels(page, labels) {
    const deadline = Date.now() + 30_000;
    let rendered = "";
    while (Date.now() < deadline) {
        rendered = (await quickPickRows(page).allTextContents()).join("\n");
        if (labels.every((label) => rendered.includes(label))) {
            return rendered;
        }
        await page.waitForTimeout(100);
    }
    throw new Error(`quick pick omitted ${labels.join(", ")}: ${rendered}`);
}

async function terminalText(terminal) {
    const renderedText = terminal.locator(".xterm-accessibility-tree, .xterm-rows");
    if (!(await renderedText.count())) {
        return "";
    }
    const lines = await renderedText.allTextContents();
    return lines.join("\n").replace(/\u00a0/g, " ");
}

async function waitForTerminalText(page, terminal, needle, description, timeout = 90_000) {
    const deadline = Date.now() + timeout;
    let rendered = "";
    while (Date.now() < deadline) {
        rendered = await terminalText(terminal);
        if (rendered.includes(needle)) {
            return rendered;
        }
        await page.waitForTimeout(250);
    }
    throw new Error(`${description} did not appear in the terminal; rendered text: ${rendered}`);
}

async function waitForTerminalTextAbsent(page, terminal, needle, description, timeout = 30_000) {
    const deadline = Date.now() + timeout;
    let rendered = "";
    while (Date.now() < deadline) {
        rendered = await terminalText(terminal);
        if (!rendered.includes(needle)) {
            return rendered;
        }
        await page.waitForTimeout(100);
    }
    throw new Error(`${description} remained in the terminal; rendered text: ${rendered}`);
}

async function focusTerminalInput(terminal) {
    const input = terminal.locator("textarea.xterm-helper-textarea");
    await input.waitFor({ state: "attached", timeout: 30_000 });
    await input.focus();
}

async function terminalHasFocus(terminal) {
    const input = terminal.locator("textarea.xterm-helper-textarea");
    return input.evaluate((element) => element.ownerDocument.activeElement === element);
}

async function typeInTerminal(page, terminal, value) {
    await focusTerminalInput(terminal);
    await page.keyboard.type(value);
}

async function pressInTerminal(page, terminal, key) {
    await focusTerminalInput(terminal);
    await page.keyboard.press(key);
}

async function waitForCodexPrompt(page, terminal, requireStartupOutput = true) {
    const marker = requireStartupOutput ? "WARNING: proceeding" : "OpenAI Codex";
    const description = requireStartupOutput ? "Codex startup output" : "restored Codex session";
    await waitForTerminalText(page, terminal, marker, description);
    for (let attempt = 0; attempt < 12; attempt += 1) {
        const rendered = await terminalText(terminal);
        for (const failure of ["error: server error", "command not found", "No such file or directory"]) {
            assert(!rendered.includes(failure), `Codex terminal reported ${failure}: ${rendered}`);
        }
        if (rendered.includes("\u203a")) {
            return;
        }
        await pressInTerminal(page, terminal, "Enter");
        await page.waitForTimeout(500);
    }
    const rendered = await terminalText(terminal);
    assert(rendered.includes("\u203a"), `Codex did not reach its input prompt: ${rendered}`);
}

async function waitForAgentFrame(page) {
    const deadline = Date.now() + 30_000;
    let frameUrls = [];
    while (Date.now() < deadline) {
        const frames = page.frames();
        frameUrls = frames.map((frame) => frame.url());
        for (const frame of frames) {
            if (!frame.url().includes("/fake.html")) {
                continue;
            }
            const body = frame.locator("body[data-rumpelpod-agent-view='true']");
            try {
                if (await body.count()) {
                    return frame;
                }
            } catch (_error) {
                // The webview can navigate while VS Code restores the sidebar.
            }
        }
        await page.waitForTimeout(100);
    }
    throw new Error(`Rumpelpod agent webview did not load; frames: ${JSON.stringify(frameUrls)}`);
}

async function waitForAgentView(page, podName, agent = "codex", expectedAgents = [agent]) {
    const frame = await waitForAgentFrame(page);
    const body = frame.locator("body[data-rumpelpod-agent-view='true']");
    const normalizedExpectedAgents = [...expectedAgents].sort();
    const deadline = Date.now() + 30_000;
    let attributes = {};
    while (Date.now() < deadline) {
        const states = {};
        for (const expectedAgent of normalizedExpectedAgents) {
            states[expectedAgent] = await body.getAttribute(
                `data-rumpelpod-${expectedAgent}-state`,
            );
        }
        attributes = {
            activeTab: await body.getAttribute("data-rumpelpod-active-tab"),
            agents: ((await body.getAttribute("data-rumpelpod-agents")) ?? "")
                .split(",")
                .filter(Boolean)
                .sort(),
            pod: await body.getAttribute("data-rumpelpod-pod"),
            states,
        };
        if (
            attributes.activeTab === agent &&
            JSON.stringify(attributes.agents) === JSON.stringify(normalizedExpectedAgents) &&
            attributes.pod === podName &&
            Object.values(attributes.states).every((state) => state === "running")
        ) {
            break;
        }
        await page.waitForTimeout(100);
    }
    assert.deepEqual(
        attributes,
        {
            activeTab: agent,
            agents: normalizedExpectedAgents,
            pod: podName,
            states: Object.fromEntries(
                normalizedExpectedAgents.map((expectedAgent) => [expectedAgent, "running"]),
            ),
        },
        `agent webview did not settle on ${podName}/${agent} with ${expectedAgents.join(", ")}`,
    );
    const podSelector = frame.locator("#pod-selector");
    await podSelector.waitFor({ state: "visible", timeout: 30_000 });
    assert.equal(
        (await frame.locator("#pod-name").textContent())?.trim(),
        podName,
        "agent view header did not show only the active pod name",
    );
    await frame.locator("#pod-chevron").waitFor({ state: "visible", timeout: 30_000 });
    for (const label of ["Open shell", "Launch agent", "Merge pod", "More pod actions"]) {
        await frame.locator(`[aria-label="${label}"]`).waitFor({
            state: "visible",
            timeout: 30_000,
        });
    }
    assert.equal(
        await frame.locator("#create-pod").count(),
        0,
        "pod-local actions still contained Create Pod",
    );
    await page
        .locator('.part.sidebar:visible [aria-label="Create Pod"]')
        .first()
        .waitFor({ state: "visible", timeout: 30_000 });
    const agentTab = frame.locator(`#${agent}-tab`);
    assert.equal((await agentTab.textContent())?.trim(), agent, "agent terminal tab was mislabeled");
    assert.equal(
        (await frame.locator("#shell-tab").textContent())?.trim(),
        "shell",
        "shell terminal tab was not rendered",
    );
    if (expectedAgents.length === 1) {
        assert.equal(
            await frame.locator("#terminal-tabs:visible").count(),
            0,
            "the session tab strip was visible before a second session opened",
        );
        assert.equal(
            await frame.locator(`#${agent}-tab:visible, #shell-tab:visible`).count(),
            0,
            "single-session mode retained a visible terminal tab",
        );
    } else {
        await frame.locator("#terminal-tabs").waitFor({ state: "visible", timeout: 30_000 });
        for (const launchedAgent of expectedAgents) {
            await frame.locator(`#${launchedAgent}-tab`).waitFor({
                state: "visible",
                timeout: 30_000,
            });
        }
    }
    const terminals = frame.locator(`#${agent}-terminal .xterm:visible`);
    await terminals.first().waitFor({ state: "visible", timeout: 30_000 });
    for (const expectedAgent of normalizedExpectedAgents) {
        const session = await body.getAttribute(`data-rumpelpod-${expectedAgent}-session`);
        assert(session, `${expectedAgent} webview did not expose its terminal session`);
        const renderedDeadline = Date.now() + 30_000;
        let renderedSession = "";
        while (Date.now() < renderedDeadline) {
            renderedSession =
                (await body.getAttribute(
                    `data-rumpelpod-${expectedAgent}-rendered-session`,
                )) ?? "";
            if (renderedSession === session) {
                break;
            }
            await page.waitForTimeout(100);
        }
        assert.equal(
            renderedSession,
            session,
            `${expectedAgent} session ${session} produced no rendered output`,
        );
    }
    assert.equal(await terminals.count(), 1, "agent tab rendered more than one xterm");
    assert.equal(
        await frame.locator("#shell-terminal .xterm:visible").count(),
        0,
        "agent tab also rendered the shell xterm",
    );
    const terminalChrome = await frame.locator(`#${agent}-terminal`).evaluate((element) => {
        const viewport = element.querySelector(".xterm-viewport");
        if (!(viewport instanceof HTMLElement)) {
            throw new Error("xterm viewport was missing");
        }
        return {
            bodyBackground: getComputedStyle(document.body).backgroundColor,
            viewportBackground: getComputedStyle(viewport).backgroundColor,
        };
    });
    assert.equal(
        terminalChrome.viewportBackground,
        terminalChrome.bodyBackground,
        "xterm viewport did not match the VS Code sidebar background",
    );
    return {
        agent,
        agentTab,
        body,
        frame,
        podSelector,
        shellTab: frame.locator("#shell-tab"),
        terminal: terminals.first(),
    };
}

async function waitForNoSelectedPod(page, view, closedPodName) {
    const body = await view.body.elementHandle();
    assert(body, "pod view body disappeared before the selection was cleared");
    try {
        await page.waitForFunction(
            (element) => element.dataset.rumpelpodPod === "",
            body,
            { timeout: 30_000 },
        );
    } finally {
        await body.dispose();
    }
    assert.equal(
        (await view.frame.locator("#pod-name").textContent())?.trim(),
        "Select pod",
        "inactive pod view did not show the selector label",
    );
    const empty = view.frame.locator("#no-sessions");
    await empty.waitFor({ state: "visible", timeout: 30_000 });
    assert.equal(
        (await empty.textContent())?.trim(),
        "Create or select a pod.",
        "inactive pod view did not explain how to continue",
    );
    for (const action of ["#open-shell", "#launch-agent", "#merge-pod", "#more-actions"]) {
        assert(
            await view.frame.locator(action).isDisabled(),
            `inactive pod view left ${action} enabled`,
        );
    }
    await page
        .locator(".tab:visible")
        .filter({ hasText: closedPodName })
        .waitFor({ state: "hidden", timeout: 30_000 });
    await page
        .locator(".statusbar-item:visible")
        .filter({ hasText: closedPodName })
        .waitFor({ state: "hidden", timeout: 30_000 });
}

async function waitForAgentRepositoryState(page, view, repositoryState) {
    const deadline = Date.now() + 30_000;
    let rendered = "";
    while (Date.now() < deadline) {
        rendered = (await view.body.getAttribute("data-rumpelpod-repository-state")) ?? "";
        if (rendered === repositoryState) {
            return;
        }
        await page.waitForTimeout(100);
    }
    throw new Error(
        `agent view did not update repository state to ${repositoryState}; rendered: ${rendered}`,
    );
}

async function openShellTab(page, view) {
    await view.frame.locator("#open-shell").click();
    const deadline = Date.now() + 30_000;
    let attributes = {};
    while (Date.now() < deadline) {
        attributes = {
            activeTab: await view.body.getAttribute("data-rumpelpod-active-tab"),
            state: await view.body.getAttribute("data-rumpelpod-shell-state"),
        };
        if (attributes.activeTab === "shell" && attributes.state === "running") {
            break;
        }
        await page.waitForTimeout(100);
    }
    assert.deepEqual(
        attributes,
        { activeTab: "shell", state: "running" },
        "embedded pod shell did not become active",
    );
    await view.frame.locator("#terminal-tabs").waitFor({ state: "visible", timeout: 30_000 });
    await view.agentTab.waitFor({ state: "visible", timeout: 30_000 });
    await view.shellTab.waitFor({ state: "visible", timeout: 30_000 });
    assert.equal(await view.shellTab.getAttribute("aria-selected"), "true");
    const terminal = view.frame.locator("#shell-terminal .xterm:visible");
    await terminal.waitFor({ state: "visible", timeout: 30_000 });
    assert.equal(
        await view.frame
            .locator(
                "#claude-terminal .xterm:visible, #codex-terminal .xterm:visible, " +
                    "#grok-terminal .xterm:visible, #pi-terminal .xterm:visible",
            )
            .count(),
        0,
        "shell tab also rendered the agent xterm",
    );
    return terminal;
}

async function openAgentTab(page, view) {
    await view.agentTab.click();
    const deadline = Date.now() + 30_000;
    let attributes = {};
    while (Date.now() < deadline) {
        attributes = {
            activeTab: await view.body.getAttribute("data-rumpelpod-active-tab"),
            state: await view.body.getAttribute(`data-rumpelpod-${view.agent}-state`),
        };
        if (attributes.activeTab === view.agent && attributes.state === "running") {
            break;
        }
        await page.waitForTimeout(100);
    }
    assert.deepEqual(
        attributes,
        { activeTab: view.agent, state: "running" },
        "agent terminal tab did not return to its running attachment",
    );
    await view.frame
        .locator(`#${view.agent}-terminal .xterm:visible`)
        .waitFor({ state: "visible", timeout: 30_000 });
}

async function openCodeServer(page, url) {
    await page.goto(url, { waitUntil: "domcontentloaded", timeout: 60_000 });
    const passwordInput = page.locator('input[name="password"]');
    assert.equal(await passwordInput.count(), 0, "code-server unexpectedly required a password");
    await page.locator(".monaco-workbench").waitFor({ state: "visible", timeout: 60_000 });
}

function shellQuote(value) {
    return `'${value.replaceAll("'", "'\"'\"'")}'`;
}

function runPodCommit(rumpel, repoRoot, home, socket, podName, changedFile, podContent) {
    const script = [
        `printf '%s\\n' ${shellQuote(podContent)} > ${shellQuote(changedFile)}`,
        `printf '%s\\n' ${shellQuote(ADDED_CONTENT)} > ${shellQuote(ADDED_FILE)}`,
        `git add -- ${shellQuote(changedFile)} ${shellQuote(ADDED_FILE)}`,
        "git commit --no-verify -m 'Change browser diff fixture'",
        "git rev-parse HEAD",
    ].join(" && ");
    const environment = {
        ...process.env,
        HOME: home,
        RUMPELPOD_DAEMON_SOCKET: socket,
    };
    const commit = spawnSync(
        rumpel,
        ["enter", "--create", podName, "--", "sh", "-c", script],
        {
            cwd: repoRoot,
            encoding: "utf8",
            env: environment,
            maxBuffer: 10 * 1024 * 1024,
        },
    );
    assert.equal(
        commit.status,
        0,
        `committing inside ${podName} failed:\nstdout:\n${commit.stdout}\nstderr:\n${commit.stderr}`,
    );
    const commits = commit.stdout.match(/[0-9a-f]{40}/g) ?? [];
    assert(commits.length > 0, `pod commit did not report its HEAD: ${commit.stdout}`);
    const podHead = commits.at(-1);
    const hostRef = spawnSync(
        "git",
        ["rev-parse", "--verify", `refs/rumpelpod/${podName}`],
        {
            cwd: repoRoot,
            encoding: "utf8",
            env: environment,
        },
    );
    assert.equal(
        hostRef.status,
        0,
        `host did not receive the ${podName} review ref:\n${hostRef.stderr}`,
    );
    assert.equal(hostRef.stdout.trim(), podHead, "host review ref did not match the pod commit");
}

function runPodFollowupCommit(rumpel, repoRoot, home, socket, podName, changedFile, podContent) {
    const script = [
        `printf '%s\\n' ${shellQuote(podContent)} > ${shellQuote(changedFile)}`,
        `git add -- ${shellQuote(changedFile)}`,
        "git commit --no-verify -m 'Update browser diff fixture'",
    ].join(" && ");
    const update = spawnSync(
        rumpel,
        ["enter", "--create", podName, "--", "sh", "-c", script],
        {
            cwd: repoRoot,
            encoding: "utf8",
            env: {
                ...process.env,
                HOME: home,
                RUMPELPOD_DAEMON_SOCKET: socket,
            },
            maxBuffer: 10 * 1024 * 1024,
        },
    );
    assert.equal(
        update.status,
        0,
        `updating ${podName} failed:\nstdout:\n${update.stdout}\nstderr:\n${update.stderr}`,
    );
}

async function waitForStatusRepository(page, podName, repositoryState) {
    const deadline = Date.now() + 30_000;
    let rendered = "";
    while (Date.now() < deadline) {
        const status = await waitForStatusItem(page, podName);
        rendered = (await status.textContent()) || "";
        if (rendered.includes(repositoryState)) {
            return;
        }
        await page.waitForTimeout(200);
    }
    throw new Error(
        `status did not update to repository state ${repositoryState}; rendered text: ${rendered}`,
    );
}

async function waitForReview(page, podName, changedFile, originalContent, podContent) {
    const diffEditors = page.locator(".monaco-diff-editor:visible");
    await diffEditors.first().waitFor({ state: "visible", timeout: 30_000 });
    await diffEditors
        .locator(".view-line")
        .filter({ hasText: originalContent })
        .first()
        .waitFor({ state: "visible", timeout: 30_000 });
    await diffEditors
        .locator(".view-line")
        .filter({ hasText: podContent })
        .first()
        .waitFor({ state: "visible", timeout: 30_000 });
    await diffEditors
        .locator(".view-line")
        .filter({ hasText: ADDED_CONTENT })
        .first()
        .waitFor({ state: "visible", timeout: 30_000 });
    assert.equal(await diffEditors.count(), 2, "review did not render every changed file");
    const rendered = (await diffEditors.locator(".view-lines").allTextContents())
        .join("\n")
        .replace(/\u00a0/g, " ");
    assert(
        rendered.includes(originalContent),
        `diff editor did not render original content; rendered text: ${rendered}`,
    );
    assert(
        rendered.includes(podContent),
        `diff editor did not render pod content; rendered text: ${rendered}`,
    );
    assert(
        rendered.includes(ADDED_CONTENT),
        `diff editor did not render added file content; rendered text: ${rendered}`,
    );
    await page.waitForFunction((pod) => document.title.includes(pod), podName, {
        timeout: 30_000,
    });
    const title = await page.title();
    assert(
        title.includes(podName),
        `active multi-diff editor did not name ${podName}: ${title}`,
    );
    assert(
        !title.includes("Rumpelpod review:"),
        `review retained the verbose title: ${title}`,
    );
    await assertEditorTabsVisible(page);
    const editorText = await page
        .locator(".editor-instance:visible")
        .filter({ has: page.locator(".monaco-diff-editor:visible") })
        .last()
        .textContent();
    assert(editorText?.includes(changedFile), `review did not label ${changedFile}`);
    assert(editorText?.includes(ADDED_FILE), `review did not label ${ADDED_FILE}`);
    return diffEditors.first();
}

async function assertEditorTabsVisible(page) {
    await page
        .locator(".editor-group-container .tabs-and-actions-container:visible")
        .first()
        .waitFor({ state: "visible", timeout: 30_000 });
    assert(
        (await page.locator(".editor-group-container .tabs-container .tab:visible").count()) > 0,
        "the extension hid VS Code's editor tabs for the entire window",
    );
}

async function waitForForwardedPreviewTabs(page, previews, podName) {
    const tabs = previews.map(({ label }) =>
        page.locator(".tab:visible").filter({ hasText: `${podName}: ${label}` }),
    );
    for (const tab of tabs) {
        await tab.waitFor({ state: "visible", timeout: 30_000 });
    }
    assert.equal(
        await page.locator(".editor-group-container:visible").count(),
        1,
        "the forwarded previews split the active editor group",
    );
    assert(
        (await page
            .locator(".editor-group-container:visible .tabs-container .tab:visible")
            .count()) >= previews.length + 1,
        "the forwarded previews did not open as tabs in the active group",
    );
    const reviewTab = page
        .locator(".tab:visible")
        .filter({ hasText: `${podName}: diff` })
        .first();
    await reviewTab.waitFor({ state: "visible", timeout: 30_000 });
    assert.equal(
        await reviewTab.evaluate((element) => element.classList.contains("active")),
        true,
        "automatic forwarded previews took focus from the review",
    );
    return { reviewTab, tabs };
}

async function waitForForwardedPreviews(page, previews, podName) {
    const { reviewTab, tabs } = await waitForForwardedPreviewTabs(page, previews, podName);
    for (const [index, preview] of previews.entries()) {
        await tabs[index].click();
        const deadline = Date.now() + 30_000;
        let loaded = false;
        while (Date.now() < deadline && !loaded) {
            for (const frame of page.frames()) {
                if (
                    await frame
                        .locator('iframe[sandbox="allow-forms allow-scripts"]')
                        .count()
                        .catch(() => 0)
                ) {
                    for (const child of frame.childFrames()) {
                        if (
                            (await child.locator("body").textContent().catch(() => ""))?.includes(
                                preview.content,
                            )
                        ) {
                            loaded = true;
                            break;
                        }
                    }
                    if (loaded) {
                        break;
                    }
                }
            }
            if (!loaded) {
                await page.waitForTimeout(100);
            }
        }
        assert(loaded, `the forwarded preview did not render: ${preview.label}`);
    }
    await reviewTab.click();
}

async function verifyReviewFocusAndCloseLifecycle(
    page,
    view,
    podName,
    changedFile,
    originalContent,
    inactivePodContent,
    afterInactive,
    closedPodContent,
    afterClose,
) {
    const activeReview = page.locator(".editor-instance:visible .multiDiffEditor");
    await activeReview.waitFor({ state: "visible", timeout: 30_000 });
    await activeReview.click();
    await page.keyboard.press("Control+P");
    const fileInput = page.locator(".quick-input-widget:visible input");
    await fileInput.waitFor({ state: "visible", timeout: 30_000 });
    await fileInput.fill(changedFile);
    await waitForQuickPickLabels(page, [changedFile]);
    await selectQuickPick(page, changedFile);
    const fileTab = page.locator(".tab:visible").filter({ hasText: changedFile }).first();
    await fileTab.waitFor({ state: "visible", timeout: 30_000 });
    assert(
        (await fileTab.getAttribute("class"))?.split(" ").includes("active"),
        "opening a normal file did not focus its editor tab",
    );

    await afterInactive();
    assert(
        (await fileTab.getAttribute("class"))?.split(" ").includes("active"),
        "a daemon review refresh stole focus from a normal file",
    );
    assert.equal(
        await page.locator(".editor-instance:visible .multiDiffEditor").count(),
        0,
        "a background review replaced the active normal file",
    );

    const moreActions = view.frame.locator("#more-actions");
    await moreActions.click();
    const morePopover = view.frame.locator("#more-popover");
    await morePopover.waitFor({ state: "visible", timeout: 30_000 });
    await selectPopoverOption(morePopover, "View diff");
    await waitForReview(page, podName, changedFile, originalContent, inactivePodContent);
    await assertOpenReviews(page, podName, [podName]);
    await fileTab.click({ button: "middle" });
    await fileTab.waitFor({ state: "hidden", timeout: 30_000 });

    const explicitlyClosedReview = page.locator(".editor-instance:visible .multiDiffEditor");
    await explicitlyClosedReview.click();
    await page.keyboard.press("Control+W");
    await page.locator(".editor-instance:visible .multiDiffEditor").waitFor({
        state: "hidden",
        timeout: 30_000,
    });

    await afterClose();
    const closedDeadline = Date.now() + 1_000;
    while (Date.now() < closedDeadline) {
        assert.equal(
            await page.locator(".editor-instance:visible .multiDiffEditor").count(),
            0,
            "a daemon review refresh reopened a review the user had closed",
        );
        await page.waitForTimeout(100);
    }

    await moreActions.click();
    await morePopover.waitFor({ state: "visible", timeout: 30_000 });
    await selectPopoverOption(morePopover, "View diff");
    await waitForReview(page, podName, changedFile, originalContent, closedPodContent);
    await assertOpenReviews(page, podName, [podName]);
    const reopenedReviewTab = page
        .locator(".tab:visible")
        .filter({ hasText: `${podName}: diff` })
        .first();
    await reopenedReviewTab.waitFor({ state: "visible", timeout: 30_000 });
}

async function waitForEmptyReview(page, podName) {
    await page.waitForFunction((pod) => document.title.includes(pod), podName, {
        timeout: 30_000,
    });
    const title = await page.title();
    assert(
        !title.includes("Rumpelpod review:"),
        `empty review retained the verbose title: ${title}`,
    );
    const emptyReview = page
        .locator(".editor-instance:visible .multiDiffEditor .placeholder.visible")
        .filter({ hasText: "No Changed Files" });
    await emptyReview.waitFor({ state: "visible", timeout: 30_000 });
    await assertEditorTabsVisible(page);
    assert.equal(
        await page.locator(".monaco-diff-editor:visible").count(),
        0,
        `the empty ${podName} review rendered a file diff`,
    );
}

async function openEditorLabels(page, expectedLabel) {
    await page.locator(".editor-instance:visible .multiDiffEditor").click();
    await page.keyboard.press("Control+P");
    const input = page.locator(".quick-input-widget:visible input");
    await input.waitFor({ state: "visible", timeout: 30_000 });
    await input.fill("edt ");
    await waitForQuickPickLabels(page, [expectedLabel]);
    const labels = await quickPickRows(page).allTextContents();
    await page.keyboard.press("Escape");
    await page
        .locator(".quick-input-widget:visible")
        .waitFor({ state: "hidden", timeout: 30_000 });
    return labels;
}

async function assertOpenReviews(page, activePodName, openPodNames) {
    const labels = await openEditorLabels(page, activePodName);
    for (const openPodName of openPodNames) {
        const matches = labels.filter((label) =>
            label.startsWith(`${openPodName}: diff`)
        );
        assert(
            matches.length === 1,
            `expected one ${openPodName} review editor: ${JSON.stringify(labels)}`,
        );
    }
}

async function assertNoNativeAgentEditor(page, podName) {
    const labels = await openEditorLabels(page, podName);
    assert(
        labels.every((label) => !label.includes("Rumpelpod:")),
        `the embedded agent was also opened as a native editor: ${JSON.stringify(labels)}`,
    );
}

async function verifyNormalFileUsesTabs(page, view, fileName) {
    await page.keyboard.press("Control+P");
    const input = page.locator(".quick-input-widget:visible input");
    await input.waitFor({ state: "visible", timeout: 30_000 });
    await input.fill(fileName);
    await waitForQuickPickLabels(page, [fileName]);
    await selectQuickPick(page, fileName);
    const fileTab = page.locator(".tab:visible").filter({ hasText: fileName }).first();
    await fileTab.waitFor({ state: "visible", timeout: 30_000 });
    await page.keyboard.press("Control+W");
    await fileTab.waitFor({ state: "hidden", timeout: 30_000 });
    const moreActions = view.frame.locator("#more-actions");
    await moreActions.click();
    const morePopover = view.frame.locator("#more-popover");
    await morePopover.waitFor({ state: "visible", timeout: 30_000 });
    await selectPopoverOption(morePopover, "View diff");
}

async function assertSidebarAndDiffGeometry(page, terminal, diffEditor) {
    const sidebar = page.locator(".part.sidebar:visible");
    const sidebarBounds = await sidebar.boundingBox();
    const terminalBounds = await terminal.boundingBox();
    const diffBounds = await diffEditor.boundingBox();
    assert(sidebarBounds, "the Rumpelpod sidebar had no visible bounds");
    assert(terminalBounds, "the agent terminal had no visible bounds");
    assert(diffBounds, "the review diff had no visible bounds");
    assert(
        sidebarBounds.x + sidebarBounds.width <= diffBounds.x + 1,
        `the review diff overlapped the Rumpelpod sidebar: ${JSON.stringify({ sidebarBounds, diffBounds })}`,
    );
    assert(
        terminalBounds.x >= sidebarBounds.x &&
            terminalBounds.x + terminalBounds.width <= sidebarBounds.x + sidebarBounds.width + 1,
        `agent xterm was not contained by the sidebar: ${JSON.stringify({ sidebarBounds, terminalBounds })}`,
    );
}

async function main() {
    const url = requiredEnvironment("RUMPELPOD_VSCODE_URL");
    const podName = requiredEnvironment("RUMPELPOD_VSCODE_POD");
    const createdPodName = requiredEnvironment("RUMPELPOD_VSCODE_CREATED_POD");
    const changedFile = requiredEnvironment("RUMPELPOD_VSCODE_CHANGED_FILE");
    const originalContent = requiredEnvironment("RUMPELPOD_VSCODE_ORIGINAL_CONTENT");
    const podContent = requiredEnvironment("RUMPELPOD_VSCODE_POD_CONTENT");
    const previewContent = requiredEnvironment("RUMPELPOD_VSCODE_PREVIEW_CONTENT");
    const secondPreviewContent = requiredEnvironment(
        "RUMPELPOD_VSCODE_SECOND_PREVIEW_CONTENT",
    );
    const inactivePodContent = `${podContent} while file active`;
    const finalPodContent = `${podContent} after close`;
    const refreshedPodContent = `${podContent} after creating another pod`;
    const repoRoot = requiredEnvironment("RUMPELPOD_VSCODE_REPO_ROOT");
    const rumpel = requiredEnvironment("RUMPELPOD_VSCODE_RUMPEL");
    const socket = requiredEnvironment("RUMPELPOD_DAEMON_SOCKET");
    const home = requiredEnvironment("RUMPELPOD_VSCODE_HOME");
    const executablePath = requiredEnvironment("RUMPELPOD_CHROMIUM");
    const artifacts = requiredEnvironment("RUMPELPOD_VSCODE_ARTIFACTS");
    const actionLog = requiredEnvironment("RUMPELPOD_VSCODE_ACTION_LOG");
    const referenceImages = process.env.RUMPELPOD_VSCODE_REFERENCE_IMAGES;

    const browser = await chromium.launch({
        executablePath,
        headless: true,
        args: ["--no-sandbox", "--disable-dev-shm-usage", "--force-renderer-accessibility"],
    });
    fs.mkdirSync(artifacts, { recursive: true });
    const context = await browser.newContext({ viewport: { width: 1600, height: 1000 } });
    await context.tracing.start({ screenshots: true, snapshots: true, sources: true });
    const page = await context.newPage();
    const browserErrors = [];
    page.on("console", (message) => {
        process.stderr.write(`[BROWSER ${message.type()}] ${message.text()}\n`);
    });
    page.on("pageerror", (error) => {
        process.stderr.write(`[BROWSER ERROR] ${error.stack || error.message}\n`);
        browserErrors.push(error);
    });

    try {
        await openCodeServer(page, url);

        await openRumpelpodView(page);
        const initialSwitcher = await waitForInitialPodSwitcher(page);
        const initialRows = await popoverRows(initialSwitcher.popover).allTextContents();
        assert(
            initialRows.some((label) => label.includes(podName)),
            `pod switcher omitted ${podName}: ${JSON.stringify(initialRows)}`,
        );
        await togglePopover(
            initialSwitcher.frame.locator("#pod-selector"),
            initialSwitcher.popover,
        );
        await assertPodSwitcherHasNoFilter(initialSwitcher.popover);
        await selectPopoverOption(initialSwitcher.popover, podName);

        const initialView = await waitForAgentView(page, podName);
        await waitForCodexPrompt(page, initialView.terminal);
        await typeInTerminal(page, initialView.terminal, "browser-input-probe");
        await waitForTerminalText(
            page,
            initialView.terminal,
            "browser-input-probe",
            "input sent through the embedded xterm",
        );
        await pressInTerminal(page, initialView.terminal, "Control+U");
        await waitForTerminalTextAbsent(
            page,
            initialView.terminal,
            "browser-input-probe",
            "cleared input sent through the embedded xterm",
        );
        await waitForEmptyReview(page, podName);
        await waitForForwardedPreviews(
            page,
            [
                { content: previewContent, label: "Browser fixture" },
                { content: secondPreviewContent, label: "Second browser fixture" },
            ],
            podName,
        );
        await assertNoNativeAgentEditor(page, podName);
        assert(
            !(await page.title()).includes("-review.txt"),
            "a clean pod opened a synthetic review document",
        );
        await verifyNormalFileUsesTabs(page, initialView, changedFile);
        await waitForEmptyReview(page, podName);
        await capture(page, artifacts, "01-default-chat.png");

        await focusTerminalInput(initialView.terminal);
        assert(await terminalHasFocus(initialView.terminal), "agent terminal did not accept focus");
        await page.waitForTimeout(100);
        runPodCommit(rumpel, repoRoot, home, socket, podName, changedFile, podContent);

        await waitForStatusRepository(page, podName, "ahead 1");
        await waitForAgentRepositoryState(page, initialView, "ahead 1");
        const liveDiff = await waitForReview(
            page,
            podName,
            changedFile,
            originalContent,
            podContent,
        );
        const liveView = await waitForAgentView(page, podName);
        await assertSidebarAndDiffGeometry(page, liveView.terminal, liveDiff);
        assert.equal(
            await liveView.frame.locator("#codex-terminal .xterm:visible").count(),
            1,
            "live review refresh created another embedded agent terminal",
        );
        assert(
            await terminalHasFocus(liveView.terminal),
            "event-driven review refresh stole focus from the agent terminal",
        );
        assert.equal(
            await page.locator('[aria-label="Select Review File"]').count(),
            0,
            "review kept the redundant file selector",
        );
        await capture(page, artifacts, "02-live-review.png");
        await verifyReviewFocusAndCloseLifecycle(
            page,
            liveView,
            podName,
            changedFile,
            originalContent,
            inactivePodContent,
            async () => {
                runPodFollowupCommit(
                    rumpel,
                    repoRoot,
                    home,
                    socket,
                    podName,
                    changedFile,
                    inactivePodContent,
                );
                await waitForStatusRepository(page, podName, "ahead 2");
                await waitForAgentRepositoryState(page, liveView, "ahead 2");
            },
            finalPodContent,
            async () => {
                runPodFollowupCommit(
                    rumpel,
                    repoRoot,
                    home,
                    socket,
                    podName,
                    changedFile,
                    finalPodContent,
                );
                await waitForStatusRepository(page, podName, "ahead 3");
                await waitForAgentRepositoryState(page, liveView, "ahead 3");
            },
        );

        const podSwitcher = await openPodSwitcher(page);
        const rowsAfterCommit = await popoverRows(podSwitcher.popover).allTextContents();
        assert(
            rowsAfterCommit.some(
                (label) =>
                    label.includes(podName) &&
                    label.includes("Codex") &&
                    label.includes("ahead 3"),
            ),
            `pod switcher omitted live agent/repository state: ${JSON.stringify(rowsAfterCommit)}`,
        );
        await capture(page, artifacts, "03-pod-switcher.png");
        await page.keyboard.press("Escape");
        await podSwitcher.popover.waitFor({ state: "hidden", timeout: 30_000 });

        const singleAgentView = await waitForAgentView(page, podName);
        const launchAgent = singleAgentView.frame.locator("#launch-agent");
        await launchAgent.click();
        const agentPopover = singleAgentView.frame.locator("#agent-popover");
        await agentPopover.waitFor({ state: "visible", timeout: 30_000 });
        await assertPopoverAnchored(launchAgent, agentPopover);
        await togglePopover(launchAgent, agentPopover);
        const agentLabels = await popoverRows(agentPopover).allTextContents();
        assert(
            agentLabels.some((label) => label.includes("Claude Code")),
            `agent launcher omitted Claude: ${JSON.stringify(agentLabels)}`,
        );
        assert(
            agentLabels.some((label) => label.includes("Codex") && label.includes("Open session")),
            `agent launcher did not mark Codex as open: ${JSON.stringify(agentLabels)}`,
        );
        assert(
            agentLabels.every((label) => !label.includes("Current agent")),
            `agent launcher retained primary-agent wording: ${JSON.stringify(agentLabels)}`,
        );
        assert.equal(
            await page.locator(".quick-input-widget:visible").count(),
            0,
            "agent launcher opened VS Code Quick Access",
        );
        await capture(page, artifacts, "04-agent-picker.png");

        const codexSessionBeforeLaunch = await singleAgentView.body.getAttribute(
            "data-rumpelpod-codex-session",
        );
        assert(codexSessionBeforeLaunch, "Codex session was missing before launching Claude");
        const codexProbe = "multi-probe";
        await typeInTerminal(page, singleAgentView.terminal, codexProbe);
        await waitForTerminalText(
            page,
            singleAgentView.terminal,
            codexProbe,
            "unsent Codex input before launching Claude",
        );
        await selectPopoverOption(agentPopover, "Claude Code");
        const claudeView = await waitForAgentView(
            page,
            podName,
            "claude",
            ["codex", "claude"],
        );
        assert.equal(
            await claudeView.body.getAttribute("data-rumpelpod-codex-session"),
            codexSessionBeforeLaunch,
            "launching Claude replaced the Codex attachment",
        );
        assert.equal(
            await claudeView.frame.locator("#codex-terminal .xterm").count(),
            1,
            "launching Claude removed or duplicated the Codex xterm",
        );
        await claudeView.frame.locator("#codex-tab").click();
        const viewWhileShellIsOpen = await waitForAgentView(
            page,
            podName,
            "codex",
            ["codex", "claude"],
        );
        await waitForTerminalText(
            page,
            viewWhileShellIsOpen.terminal,
            codexProbe,
            "Codex input preserved while Claude was launched",
        );
        await pressInTerminal(page, viewWhileShellIsOpen.terminal, "Control+U");
        await waitForTerminalTextAbsent(
            page,
            viewWhileShellIsOpen.terminal,
            codexProbe,
            "cleared Codex input after launching Claude",
        );

        const codexSessionBeforeShell = await viewWhileShellIsOpen.body.getAttribute(
            "data-rumpelpod-codex-session",
        );
        assert(codexSessionBeforeShell, "Codex session was missing before opening the shell");
        const shellTerminal = await openShellTab(page, viewWhileShellIsOpen);
        const shellSessionBeforeSwitch = await viewWhileShellIsOpen.body.getAttribute(
            "data-rumpelpod-shell-session",
        );
        assert(shellSessionBeforeSwitch, "shell session was missing after opening the shell");
        const shellProbe = "embedded-shell-probe";
        await typeInTerminal(
            page,
            shellTerminal,
            "export RUMPELPOD_BROWSER_SHELL_MARKER=olive; printf 'embedded-%s\\n' shell-probe",
        );
        await pressInTerminal(page, shellTerminal, "Enter");
        await waitForTerminalText(
            page,
            shellTerminal,
            shellProbe,
            "command output from the embedded pod shell",
        );
        assert.equal(
            await page.locator(".part.panel:visible .terminal-wrapper .xterm:visible").count(),
            0,
            "pod shell opened in VS Code's native terminal panel",
        );
        await capture(page, artifacts, "09-shell.png");
        await openAgentTab(page, viewWhileShellIsOpen);
        assert.equal(
            await viewWhileShellIsOpen.body.getAttribute("data-rumpelpod-codex-session"),
            codexSessionBeforeShell,
            "opening the shell replaced the agent attachment",
        );
        assert.equal(
            await viewWhileShellIsOpen.body.getAttribute("data-rumpelpod-shell-session"),
            shellSessionBeforeSwitch,
            "leaving the shell tab replaced its process",
        );
        await waitForCodexPrompt(page, viewWhileShellIsOpen.terminal, false);
        const preservedAgentProbe = "preserved-agent-probe";
        await typeInTerminal(page, viewWhileShellIsOpen.terminal, preservedAgentProbe);
        await waitForTerminalText(
            page,
            viewWhileShellIsOpen.terminal,
            preservedAgentProbe,
            "input handled by the agent after leaving the shell tab",
        );
        await pressInTerminal(page, viewWhileShellIsOpen.terminal, "Control+U");
        await waitForTerminalTextAbsent(
            page,
            viewWhileShellIsOpen.terminal,
            preservedAgentProbe,
            "cleared agent input after leaving the shell tab",
        );
        const restoredShell = await openShellTab(page, viewWhileShellIsOpen);
        assert.equal(
            await viewWhileShellIsOpen.body.getAttribute("data-rumpelpod-codex-session"),
            codexSessionBeforeShell,
            "returning to the shell replaced the agent attachment",
        );
        assert.equal(
            await viewWhileShellIsOpen.body.getAttribute("data-rumpelpod-shell-session"),
            shellSessionBeforeSwitch,
            "returning to the shell started a new process",
        );
        await waitForTerminalText(
            page,
            restoredShell,
            shellProbe,
            "preserved shell output after switching terminal tabs",
        );
        const preservedShellProbe = "session-olive";
        await typeInTerminal(
            page,
            restoredShell,
            "printf 'session-%s\\n' \"$RUMPELPOD_BROWSER_SHELL_MARKER\"",
        );
        await pressInTerminal(page, restoredShell, "Enter");
        await waitForTerminalText(
            page,
            restoredShell,
            preservedShellProbe,
            "environment retained by the shell process across terminal tabs",
        );
        await typeInTerminal(page, restoredShell, "exit");
        await pressInTerminal(page, restoredShell, "Enter");
        await restoredShell.waitFor({ state: "hidden", timeout: 30_000 });
        const agentBodyHandle = await viewWhileShellIsOpen.body.elementHandle();
        assert(agentBodyHandle, "agent view body was missing after the shell exited");
        await page.waitForFunction(
            (body) => body.dataset.rumpelpodActiveTab === "codex",
            agentBodyHandle,
            { timeout: 30_000 },
        );
        await agentBodyHandle.dispose();
        await viewWhileShellIsOpen.frame
            .locator("#terminal-tabs")
            .waitFor({ state: "visible", timeout: 30_000 });
        await viewWhileShellIsOpen.frame
            .locator("#codex-tab")
            .waitFor({ state: "visible", timeout: 30_000 });
        await viewWhileShellIsOpen.frame
            .locator("#claude-tab")
            .waitFor({ state: "visible", timeout: 30_000 });
        await viewWhileShellIsOpen.frame
            .locator("#shell-tab")
            .waitFor({ state: "hidden", timeout: 30_000 });
        await viewWhileShellIsOpen.terminal.waitFor({ state: "visible", timeout: 30_000 });

        const moreActions = viewWhileShellIsOpen.frame.locator("#more-actions");
        await moreActions.click();
        const morePopover = viewWhileShellIsOpen.frame.locator("#more-popover");
        await morePopover.waitFor({ state: "visible", timeout: 30_000 });
        await assertPopoverAnchored(moreActions, morePopover);
        await togglePopover(moreActions, morePopover);
        const moreLabels = await popoverRows(morePopover).allTextContents();
        assert(
            moreLabels.some((label) => label.includes("View diff")) &&
                moreLabels.some((label) => label.includes("Open port in VS Code")) &&
                moreLabels.some((label) => label.includes("Open port in browser")) &&
                moreLabels.some((label) => label.includes("Refresh pod")) &&
                moreLabels.some((label) => label.includes("Add SSH key")) &&
                moreLabels.some((label) => label.includes("Stop pod")) &&
                moreLabels.some((label) => label.includes("Delete pod")) &&
                moreLabels.every((label) => !label.includes("Restart")),
            `pod action menu omitted commands: ${JSON.stringify(moreLabels)}`,
        );
        assert.equal(
            await morePopover.locator('[role="separator"]').count(),
            2,
            "pod action menu did not group secondary and lifecycle actions",
        );
        await selectPopoverOption(morePopover, "Open port in VS Code");
        await selectQuickPick(page, "Browser fixture");
        const explicitPreview = page
            .locator(".tab.active:visible")
            .filter({ hasText: `${podName}: Browser fixture` });
        await explicitPreview.waitFor({ state: "visible", timeout: 30_000 });
        assert.equal(
            await explicitPreview.count(),
            1,
            "explicitly opening a forwarded preview did not focus its tab",
        );

        await moreActions.click();
        await morePopover.waitFor({ state: "visible", timeout: 30_000 });
        const externalPagePromise = context.waitForEvent("page", { timeout: 30_000 });
        await selectPopoverOption(morePopover, "Open port in browser");
        await selectQuickPick(page, "Browser fixture");
        const externalPage = await externalPagePromise;
        await externalPage.locator("body").filter({ hasText: previewContent }).waitFor({
            state: "visible",
            timeout: 30_000,
        });
        assert(
            externalPage.url().startsWith("http://") || externalPage.url().startsWith("https://"),
            `external forwarded port used an unexpected URL: ${externalPage.url()}`,
        );
        await externalPage.close();

        const createPod = page
            .locator('.part.sidebar:visible [aria-label="Create Pod"]')
            .first();
        await createPod.click();
        const createPopover = viewWhileShellIsOpen.frame.locator("#create-popover");
        await createPopover.waitFor({ state: "visible", timeout: 30_000 });
        await assertNativePopoverAnchored(createPod, createPopover);
        await toggleNativeCreatePopover(createPod, createPopover);
        assert.equal(
            await page.locator(".quick-input-widget:visible").count(),
            0,
            "pod creation opened VS Code Quick Access",
        );
        const agentOptions = await createPopover.locator("#pod-agent-input option").allTextContents();
        assert(agentOptions.includes("Claude Code"), `pod creation omitted Claude: ${agentOptions}`);
        assert(agentOptions.includes("Codex"), `pod creation omitted Codex: ${agentOptions}`);
        const podNameInput = createPopover.locator("#pod-name-input");
        await podNameInput.waitFor({ state: "visible", timeout: 30_000 });
        assert.equal(
            await podNameInput.getAttribute("placeholder"),
            "feature-name",
            "pod creation did not advance to the name input",
        );
        await podNameInput.fill(createdPodName);
        await capture(page, artifacts, "05-pod-name.png");
        const previousPodTerminal = await viewWhileShellIsOpen.terminal.elementHandle();
        assert(previousPodTerminal, "existing pod terminal was missing before creating a pod");
        await createPopover.locator('button[type="submit"]').click();
        await createPopover.waitFor({ state: "hidden", timeout: 30_000 });

        const createdView = await waitForAgentView(page, createdPodName);
        await waitForCodexPrompt(page, createdView.terminal);
        await page.waitForFunction(
            (terminal) => terminal.isConnected && terminal.getClientRects().length === 0,
            previousPodTerminal,
            { timeout: 30_000 },
        );
        await waitForPersistedPodStatus(page, createdPodName);
        assert.equal(
            await page.locator(".part.panel:visible .terminal-wrapper .xterm:visible").count(),
            0,
            "creating a pod kept the closed auxiliary shell alive",
        );
        await waitForEmptyReview(page, createdPodName);
        await waitForForwardedPreviewTabs(
            page,
            [
                { content: previewContent, label: "Browser fixture" },
                { content: secondPreviewContent, label: "Second browser fixture" },
            ],
            createdPodName,
        );
        await assertOpenReviews(page, createdPodName, [createdPodName]);
        assert(
            !(await page.title()).includes("-review.txt"),
            "creating a pod opened a synthetic review document",
        );
        await capture(page, artifacts, "06-created-chat.png");

        const switchedPodSwitcher = await openPodSwitcher(page);
        const listedPods = await popoverRows(switchedPodSwitcher.popover).allTextContents();
        assert(
            listedPods.some((label) => label.includes(podName)),
            `existing pod disappeared from the switcher: ${JSON.stringify(listedPods)}`,
        );
        assert(
            listedPods.some((label) => label.includes(createdPodName)),
            `created pod was not listed in the switcher: ${JSON.stringify(listedPods)}`,
        );
        const createdPodTerminal = await createdView.terminal.elementHandle();
        assert(createdPodTerminal, "created pod terminal was missing before switching pods");
        await selectPopoverOption(switchedPodSwitcher.popover, podName);
        const switchedView = await waitForAgentView(
            page,
            podName,
            "codex",
            ["codex", "claude"],
        );
        await waitForCodexPrompt(page, switchedView.terminal, false);
        await page.waitForFunction(
            (terminal) => terminal.isConnected && terminal.getClientRects().length === 0,
            createdPodTerminal,
            { timeout: 30_000 },
        );
        await createdPodTerminal.dispose();
        await page.waitForFunction(
            (terminal) => terminal.isConnected && terminal.getClientRects().length > 0,
            previousPodTerminal,
            { timeout: 30_000 },
        );
        const restoredPodTerminal = await switchedView.terminal.elementHandle();
        assert(restoredPodTerminal, "restored pod terminal was missing after switching pods");
        assert(
            await previousPodTerminal.evaluate(
                (previous, restored) => previous === restored,
                restoredPodTerminal,
            ),
            "switching back replaced the pod's terminal and scrollback",
        );
        await restoredPodTerminal.dispose();
        await previousPodTerminal.dispose();
        await waitForReview(
            page,
            podName,
            changedFile,
            originalContent,
            finalPodContent,
        );
        runPodFollowupCommit(
            rumpel,
            repoRoot,
            home,
            socket,
            podName,
            changedFile,
            refreshedPodContent,
        );
        await waitForStatusRepository(page, podName, "ahead 4");
        await waitForAgentRepositoryState(page, switchedView, "ahead 4");
        const switchedDiff = await waitForReview(
            page,
            podName,
            changedFile,
            originalContent,
            refreshedPodContent,
        );
        const activeRestoredReview = page
            .locator(".tab.active:visible")
            .filter({ hasText: `${podName}: diff` });
        assert(
            (await activeRestoredReview.count()) === 1,
            "switching pods did not focus its review tab",
        );
        await assertOpenReviews(page, podName, [podName]);
        await assertSidebarAndDiffGeometry(page, switchedView.terminal, switchedDiff);
        assert.equal(
            await switchedView.body.getAttribute("data-rumpelpod-pod"),
            podName,
            "switching pods left the previous pod's sessions in the embedded terminal",
        );
        await capture(page, artifacts, "07-switched-review.png");

        const reloadMarker = "reload-session-marker";
        await typeInTerminal(page, switchedView.terminal, reloadMarker);
        await waitForTerminalText(
            page,
            switchedView.terminal,
            reloadMarker,
            "unsent input before browser reload",
        );

        await page.reload({ waitUntil: "domcontentloaded", timeout: 60_000 });
        await page.locator(".monaco-workbench").waitFor({ state: "visible", timeout: 60_000 });
        await page
            .locator(".part.sidebar:visible .composite.title")
            .filter({ hasText: "Rumpelpod" })
            .waitFor({ state: "visible", timeout: 30_000 });
        await page
            .locator('.activitybar a[aria-label="Rumpelpod"]')
            .waitFor({ state: "visible", timeout: 30_000 });
        await waitForStatusItem(page, podName);
        const restoredView = await waitForAgentView(
            page,
            podName,
            "codex",
            ["codex", "claude"],
        );
        await waitForTerminalText(
            page,
            restoredView.terminal,
            reloadMarker,
            "the attached Codex session after browser reload",
        );
        await pressInTerminal(page, restoredView.terminal, "Control+U");
        await waitForTerminalTextAbsent(
            page,
            restoredView.terminal,
            reloadMarker,
            "cleared input after browser reload",
        );
        await waitForCodexPrompt(page, restoredView.terminal, false);
        const restoredReviewTab = page
            .locator(".tab:visible")
            .filter({ hasText: `${podName}: diff` })
            .first();
        await restoredReviewTab.waitFor({ state: "visible", timeout: 30_000 });
        await restoredReviewTab.click();
        const restoredDiff = await waitForReview(
            page,
            podName,
            changedFile,
            originalContent,
            refreshedPodContent,
        );
        await assertSidebarAndDiffGeometry(page, restoredView.terminal, restoredDiff);
        assert.equal(
            await restoredView.frame.locator("#codex-terminal .xterm:visible").count(),
            1,
            "reload restored more than one embedded agent terminal",
        );
        await capture(page, artifacts, "08-restored-chat.png");

        const selectedMoreActions = restoredView.frame.locator("#more-actions");
        await selectedMoreActions.click();
        const selectedMorePopover = restoredView.frame.locator("#more-popover");
        await selectedMorePopover.waitFor({ state: "visible", timeout: 30_000 });
        await selectPopoverOption(selectedMorePopover, "Add SSH key...");
        const keyPicker = page.locator(".quick-input-widget:visible");
        await keyPicker.waitFor({ state: "visible", timeout: 30_000 });
        assert(
            ((await keyPicker.textContent()) ?? "").includes("Add SSH key") ||
                ((await keyPicker.getAttribute("aria-label")) ?? "").includes("Add SSH key"),
            "SSH key action did not open a file-selection dialog",
        );
        const sshKeyName = "id-vscode-test";
        const sshKeyPath = path.join(home, ".ssh", sshKeyName);
        const sshKeyRow = quickPickRows(page).filter({ hasText: sshKeyName }).first();
        await sshKeyRow.waitFor({ state: "visible", timeout: 30_000 });
        await sshKeyRow.click();
        await keyPicker.waitFor({ state: "hidden", timeout: 30_000 });
        const passphraseDialog = page.locator(".quick-input-widget:visible");
        const passphraseInput = passphraseDialog.locator('input[type="password"]');
        await passphraseInput.waitFor({ state: "visible", timeout: 30_000 });
        await passphraseInput.fill("vscode-passphrase");
        await page.keyboard.press("Enter");
        await passphraseDialog.waitFor({ state: "hidden", timeout: 30_000 });
        const sshAdded = page.getByText(`Added SSH key to pod '${podName}'.`, { exact: true });
        await sshAdded.waitFor({ state: "visible", timeout: 30_000 });
        await waitForAction(actionLog, `ssh-add ${podName} ${sshKeyPath}`, 2);

        await restoredView.frame.locator("#merge-pod").click();
        await confirmModal(page, `Merge pod '${podName}'`, "Merge Pod");
        await waitForAction(actionLog, `merge ${podName} --no-edit`);
        await waitForNoSelectedPod(page, restoredView, podName);

        const lifecycleSwitcher = await openPodSwitcher(page);
        await selectPopoverOption(lifecycleSwitcher.popover, createdPodName);
        const lifecycleView = await waitForAgentView(page, createdPodName);
        const lifecycleMore = lifecycleView.frame.locator("#more-actions");
        await lifecycleMore.click();
        const lifecyclePopover = lifecycleView.frame.locator("#more-popover");
        await lifecyclePopover.waitFor({ state: "visible", timeout: 30_000 });
        await selectPopoverOption(lifecyclePopover, "Stop pod");
        await waitForAction(actionLog, `stop --wait ${createdPodName}`);
        await waitForNoSelectedPod(page, lifecycleView, createdPodName);

        const stoppedPodSwitcher = await openPodSwitcher(page);
        const stoppedPodLabels = await popoverRows(stoppedPodSwitcher.popover).allTextContents();
        assert(
            stoppedPodLabels.some(
                (label) =>
                    label.includes(createdPodName) && label.toLowerCase().includes("stopped"),
            ),
            `stopped pod state was not reflected in the selector: ${JSON.stringify(stoppedPodLabels)}`,
        );
        await selectPopoverOption(stoppedPodSwitcher.popover, createdPodName);
        const deleteView = await waitForAgentView(page, createdPodName);
        const deleteMore = deleteView.frame.locator("#more-actions");
        await deleteMore.click();
        const deletePopover = deleteView.frame.locator("#more-popover");
        await deletePopover.waitFor({ state: "visible", timeout: 30_000 });
        await selectPopoverOption(deletePopover, "Delete pod...");
        await confirmModal(page, `Permanently delete pod '${createdPodName}'`, "Delete Pod");
        await waitForAction(actionLog, `delete --wait --force ${createdPodName}`);
        await waitForNoSelectedPod(page, deleteView, createdPodName);
        const remainingPods = await openPodSwitcher(page);
        const remainingPodLabels = await popoverRows(remainingPods.popover).allTextContents();
        assert(
            remainingPodLabels.every((label) => !label.includes(createdPodName)),
            `deleted pod remained in selector: ${JSON.stringify(remainingPodLabels)}`,
        );
        await page.keyboard.press("Escape");
        await remainingPods.popover.waitFor({ state: "hidden", timeout: 30_000 });
        assert.equal(
            browserErrors.length,
            0,
            `browser page errors occurred: ${browserErrors.map((error) => error.message).join("; ")}`,
        );
        publishReferenceImages(artifacts, referenceImages);
    } catch (error) {
        fs.mkdirSync(artifacts, { recursive: true });
        try {
            await page.screenshot({
                path: path.join(artifacts, "failure.png"),
                fullPage: true,
            });
        } catch (screenshotError) {
            process.stderr.write(`capturing failure screenshot failed: ${screenshotError}\n`);
        }
        throw error;
    } finally {
        try {
            await context.tracing.stop({ path: path.join(artifacts, "trace.zip") });
            process.stderr.write(`Playwright artifacts: ${artifacts}\n`);
        } catch (traceError) {
            process.stderr.write(`capturing Playwright trace failed: ${traceError}\n`);
        }
        await browser.close();
    }
}

main().catch((error) => {
    process.stderr.write(`${error.stack || error}\n`);
    process.exitCode = 1;
});
