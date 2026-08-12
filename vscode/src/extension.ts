// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

import * as vscode from "vscode";

import { RumpelpodController } from "./controller";
import { RumpelpodDaemon } from "./daemon";
import { DaemonEvents } from "./events";
import { RumpelpodModel } from "./model";
import { PortPreviews } from "./ports";
import { ReviewDocuments } from "./review";
import { AGENT_VIEW_ID, AgentTerminals, type AgentViewAction } from "./terminal";

export function activate(context: vscode.ExtensionContext): void {
  const output = vscode.window.createOutputChannel("Rumpelpod", { log: true });
  const daemon = new RumpelpodDaemon((message) => output.appendLine(message));
  const model = new RumpelpodModel(
    context.workspaceState,
    output,
    context.extensionUri,
    daemon,
  );
  const terminals = new AgentTerminals(context.extensionUri, (operation, error) => {
    model.logError(operation, error);
  });
  const reviews = new ReviewDocuments((operation, error) => {
    model.logError(operation, error);
  });
  const previews = new PortPreviews();
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  status.name = "Rumpelpod Active Pod";
  status.command = "rumpelpod.showPods";
  const controller = new RumpelpodController(model, terminals, reviews, previews, status);
  const events = new DaemonEvents(daemon, model, controller);

  context.subscriptions.push(
    output,
    terminals,
    reviews,
    previews,
    status,
    controller,
    events,
    terminals.onDidShow(() => {
      void runCommand(model, "opening the Rumpelpod view", () => controller.enterMode());
    }),
    terminals.onDidRequestAction((action) => {
      void runCommand(
        model,
        viewActionContext(action),
        () => controller.handleViewAction(action),
        () => dismissFailedMenu(terminals, action),
      );
    }),
    vscode.window.registerWebviewViewProvider(AGENT_VIEW_ID, terminals, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    vscode.commands.registerCommand("rumpelpod.showPods", () =>
      runCommand(model, "selecting a pod", () => controller.showPodSwitcher()),
    ),
    vscode.commands.registerCommand("rumpelpod.refresh", () =>
      runCommand(model, "refreshing the active pod", () => controller.refresh(true)),
    ),
    vscode.commands.registerCommand("rumpelpod.createPod", () =>
      runCommand(model, "creating a pod", () => controller.createPod()),
    ),
    vscode.commands.registerCommand("rumpelpod.launchAgent", () =>
      runCommand(model, "launching an agent", () => controller.launchAgentMenu()),
    ),
    vscode.commands.registerCommand("rumpelpod.openActiveShell", () =>
      runCommand(model, "opening a pod shell", async () => controller.openActiveShell()),
    ),
    vscode.commands.registerCommand("rumpelpod.openForwardedPort", () =>
      runCommand(model, "opening a forwarded port in VS Code", () =>
        controller.openForwardedPort(false)),
    ),
    vscode.commands.registerCommand("rumpelpod.openForwardedPortExternal", () =>
      runCommand(model, "opening a forwarded port in an external browser", () =>
        controller.openForwardedPort(true)),
    ),
    vscode.commands.registerCommand("rumpelpod.mergeActivePod", () =>
      runCommand(model, "merging the active pod", () => controller.mergeActivePod()),
    ),
    vscode.commands.registerCommand("rumpelpod.addSshKey", () =>
      runCommand(model, "adding an SSH key", () => controller.addSshKey()),
    ),
    vscode.commands.registerCommand("rumpelpod.stopActivePod", () =>
      runCommand(model, "stopping the active pod", () => controller.stopActivePod()),
    ),
    vscode.commands.registerCommand("rumpelpod.deleteActivePod", () =>
      runCommand(model, "deleting the active pod", () => controller.deleteActivePod()),
    ),
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      model.invalidateRepositories();
      events.restart();
    }),
  );

  controller.updateStatus();
  void vscode.commands.executeCommand("setContext", "rumpelpod.hasActivePod", false);
  void events.start().catch((error: unknown) => {
    model.logError("starting rumpelpod daemon events", error);
    events.restart();
  });
}

function dismissFailedMenu(terminals: AgentTerminals, action: AgentViewAction): void {
  switch (action.type) {
    case "launchAgentMenu":
    case "createPodMenu":
    case "podMenu":
      terminals.dismissMenu(action.request);
      return;
    case "createPod":
    case "addSshKey":
    case "openPod":
    case "openPort":
    case "openPortExternal":
    case "openShell":
    case "refresh":
    case "launchAgent":
    case "deletePod":
    case "mergePod":
    case "stopPod":
    case "viewDiff":
      return;
  }
}

function viewActionContext(action: AgentViewAction): string {
  switch (action.type) {
    case "launchAgentMenu":
      return "opening the agent selector";
    case "addSshKey":
      return "adding an SSH key";
    case "createPod":
      return "creating a pod";
    case "createPodMenu":
      return "opening pod creation";
    case "deletePod":
      return "deleting the active pod";
    case "mergePod":
      return "merging the active pod";
    case "openPod":
      return "selecting a pod";
    case "openPort":
      return "opening a forwarded port in VS Code";
    case "openPortExternal":
      return "opening a forwarded port in an external browser";
    case "openShell":
      return "opening a pod shell";
    case "podMenu":
      return "listing pods";
    case "refresh":
      return "refreshing the active pod";
    case "launchAgent":
      return "launching an agent";
    case "stopPod":
      return "stopping the active pod";
    case "viewDiff":
      return "opening the active pod diff";
  }
}

async function runCommand(
  model: RumpelpodModel,
  context: string,
  command: () => Promise<void>,
  onError?: () => void,
): Promise<void> {
  try {
    await command();
  } catch (error) {
    onError?.();
    model.logError(context, error);
    await vscode.window.showErrorMessage(`Rumpelpod failed while ${context}: ${errorMessage(error)}`);
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
