// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

import * as vscode from "vscode";

interface MoveActiveEditorArguments {
  readonly by: "tab";
  readonly to: "last" | "position";
  readonly value?: number;
}

export async function moveActiveTabToEnd(
  matches: (tab: vscode.Tab) => boolean,
): Promise<vscode.Tab> {
  return await moveActiveTab(matches, { by: "tab", to: "last" });
}

export async function moveActiveTabToPosition(
  matches: (tab: vscode.Tab) => boolean,
  position: number,
): Promise<vscode.Tab> {
  if (!Number.isSafeInteger(position) || position < 0) {
    throw new Error(`invalid zero-based tab position ${position}`);
  }
  return await moveActiveTab(matches, {
    by: "tab",
    to: "position",
    value: position + 1,
  });
}

async function moveActiveTab(
  matches: (tab: vscode.Tab) => boolean,
  destination: MoveActiveEditorArguments,
): Promise<vscode.Tab> {
  const active = await waitForActiveTab(matches);
  let group = vscode.window.tabGroups.activeTabGroup;
  let destinationIndex = tabDestinationIndex(group, destination);
  if (group.tabs[destinationIndex] === active) {
    return active;
  }

  await vscode.commands.executeCommand("moveActiveEditor", destination);
  group = vscode.window.tabGroups.activeTabGroup;
  destinationIndex = tabDestinationIndex(group, destination);
  const moved = group.activeTab;
  if (
    moved === undefined ||
    !matches(moved) ||
    group.tabs[destinationIndex] !== moved
  ) {
    throw new Error("VS Code did not move the active tab to its requested position");
  }
  return moved;
}

function tabDestinationIndex(
  group: vscode.TabGroup,
  destination: MoveActiveEditorArguments,
): number {
  switch (destination.to) {
    case "last":
      return group.tabs.length - 1;
    case "position": {
      const position = destination.value;
      if (
        position === undefined ||
        !Number.isSafeInteger(position) ||
        position < 1 ||
        position > group.tabs.length
      ) {
        throw new Error(`tab position ${position} is outside the active editor group`);
      }
      return position - 1;
    }
  }
}

export async function waitForActiveTab(
  matches: (tab: vscode.Tab) => boolean,
): Promise<vscode.Tab> {
  const active = vscode.window.tabGroups.activeTabGroup.activeTab;
  if (active !== undefined && matches(active)) {
    return active;
  }

  return await new Promise<vscode.Tab>((resolve, reject) => {
    const timeout = setTimeout(() => {
      subscription.dispose();
      reject(new Error("VS Code did not activate the tab before moving it"));
    }, 5_000);
    const subscription = vscode.window.tabGroups.onDidChangeTabs(() => {
      const changed = vscode.window.tabGroups.activeTabGroup.activeTab;
      if (changed === undefined || !matches(changed)) {
        return;
      }
      clearTimeout(timeout);
      subscription.dispose();
      resolve(changed);
    });
    const changed = vscode.window.tabGroups.activeTabGroup.activeTab;
    if (changed !== undefined && matches(changed)) {
      clearTimeout(timeout);
      subscription.dispose();
      resolve(changed);
    }
  });
}
