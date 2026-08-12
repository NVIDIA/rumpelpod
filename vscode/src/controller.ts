// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

import * as os from "node:os";
import * as path from "node:path";
import * as vscode from "vscode";

import type { PodInfo, PortInfo } from "./generated/protocol";
import { AGENTS, agentDescription, agentLabel, type AgentKind } from "./agents";
import { SshPassphraseRequiredError, type Repository, type RumpelpodModel } from "./model";
import type { PortPreviews } from "./ports";
import type { ReviewDocuments } from "./review";
import type {
  AgentMenuOption,
  AgentTerminals,
  AgentViewAction,
  PodMenuOption,
} from "./terminal";

interface ActivePod {
  readonly agents: readonly AgentKind[];
  readonly generation: number;
  readonly info: PodInfo | undefined;
  readonly pod: string;
  readonly repository: Repository;
}

interface PortPick extends vscode.QuickPickItem {
  readonly port: PortInfo;
}

export class RumpelpodController implements vscode.Disposable {
  private active: ActivePod | undefined;
  private agentLaunches = Promise.resolve();
  private automaticPreviewAttemptGeneration: number | undefined;
  private automaticPreviewGeneration: number | undefined;
  private modeEntries = Promise.resolve();
  private podOperations = Promise.resolve();
  private reviewUpdates = Promise.resolve();
  private selectionGeneration = 0;
  private statusUpdates = Promise.resolve();
  private readonly tabSubscription: vscode.Disposable;

  public constructor(
    private readonly model: RumpelpodModel,
    private readonly terminals: AgentTerminals,
    private readonly reviewDocuments: ReviewDocuments,
    private readonly portPreviews: PortPreviews,
    private readonly status: vscode.StatusBarItem,
  ) {
    this.tabSubscription = vscode.window.tabGroups.onDidChangeTabs((event) => {
      const selected = this.active;
      if (selected === undefined) {
        return;
      }
      const reviewTab = this.reviewDocuments.activeReviewTab(
        selected.repository,
        selected.pod,
      );
      if (
        reviewTab === undefined ||
        ![...event.opened, ...event.changed].some((tab) => tab === reviewTab)
      ) {
        return;
      }
      void this.tryAutomaticPortPreview(selected);
    });
  }

  public dispose(): void {
    this.tabSubscription.dispose();
  }

  public async showPodSwitcher(request?: number): Promise<void> {
    const menuRequest = await this.terminals.beginMenu("pod", request);
    try {
      const repositories = await this.model.repositories();
      if (repositories.length === 0) {
        throw new Error("open a Git repository before selecting a pod");
      }

      const picks = (
        await Promise.all(
          repositories.map(async (repository) => {
            const pods = await this.model.listPods(repository, false);
            return pods.map((pod) => this.podPick(repository, pod, repositories.length > 1));
          }),
        )
      ).flat();
      await this.terminals.showPodMenu(picks, menuRequest);
    } catch (error) {
      this.terminals.dismissMenu(menuRequest);
      throw error;
    }
  }

  public enterMode(): Promise<void> {
    const result = this.modeEntries.then(async () => {
      if (this.active === undefined) {
        const restored = await this.restoreLastPod();
        if (restored) {
          return;
        }
      }
      const selected = this.active;
      if (selected === undefined) {
        await this.showPodSwitcher();
        return;
      }
      await this.terminals.showActive(
        selected.repository,
        selected.pod,
        selected.agents,
        this.model.executable(),
      );
      this.updateStatus();
      await this.revealActiveReview();
      await this.tryAutomaticPortPreview(selected);
    });
    this.modeEntries = result.catch(() => {});
    return result;
  }

  public async refresh(sync: boolean): Promise<void> {
    const failures = (
      await Promise.allSettled([
        this.refreshPodStatus(sync),
        this.refreshActiveReview(),
      ])
    )
      .filter((result): result is PromiseRejectedResult => result.status === "rejected")
      .map((result) => result.reason);
    if (failures.length === 1) {
      throw failures[0];
    }
    if (failures.length > 1) {
      throw new AggregateError(failures, "rumpelpod status and review refreshes failed");
    }
  }

  public refreshPodStatus(sync: boolean): Promise<void> {
    const selected = this.active;
    return this.enqueueStatusUpdate(async () => {
      if (selected === undefined) {
        if (this.active === undefined) {
          this.updateStatus();
        }
        return;
      }
      const pods = await this.model.listPods(selected.repository, sync);
      const active = this.currentSelection(selected);
      if (active === undefined) {
        return;
      }
      const info = pods.find((pod) => pod.name === selected.pod);
      this.active = { ...active, info };
      this.updateStatus();
      await this.tryAutomaticPortPreview(this.active);
    });
  }

  public refreshActiveReview(): Promise<void> {
    return this.updateActiveReview(false);
  }

  public revealActiveReview(): Promise<void> {
    return this.updateActiveReview(true);
  }

  private updateActiveReview(reveal: boolean): Promise<void> {
    const selected = this.active;
    return this.enqueueReviewUpdate(async () => {
      if (selected === undefined) {
        return;
      }
      const plan = await this.model.review(selected.repository, selected.pod);
      const active = this.currentSelection(selected);
      if (active === undefined) {
        return;
      }
      const restoreTerminalFocus = !reveal && this.terminals.hasFocus();
      try {
        if (plan.files.length === 0) {
          if (reveal) {
            await this.reviewDocuments.revealEmpty(selected.repository, selected.pod);
          } else {
            await this.reviewDocuments.refreshEmpty(selected.repository, selected.pod);
          }
          return;
        }
        if (reveal) {
          await this.reviewDocuments.reveal(selected.repository, selected.pod, plan);
        } else {
          await this.reviewDocuments.refresh(selected.repository, selected.pod, plan);
        }
      } finally {
        if (restoreTerminalFocus) {
          await this.terminals.restoreFocus();
        }
      }
    });
  }

  public isActivePod(repositoryRoot: string, pod: string): boolean {
    return this.active?.repository.root === repositoryRoot && this.active.pod === pod;
  }

  public async createPod(request?: number): Promise<void> {
    const menuRequest = await this.terminals.beginMenu("create", request);
    try {
      const repositories = await this.model.repositories();
      if (repositories.length === 0) {
        throw new Error("open a Git repository before creating a pod");
      }
      await this.terminals.showCreatePodMenu(
        repositories.map((repository) => ({
          name: repository.name,
          root: repository.root,
        })),
        this.agentOptions(this.model.defaultAgent(), []),
        menuRequest,
      );
    } catch (error) {
      this.terminals.dismissMenu(menuRequest);
      throw error;
    }
  }

  public async handleViewAction(action: AgentViewAction): Promise<void> {
    switch (action.type) {
      case "launchAgentMenu":
        await this.launchAgentMenu(action.request);
        return;
      case "addSshKey":
        await this.addSshKey();
        return;
      case "createPod":
        await this.createPodSelection(action.repository, action.agent, action.pod);
        return;
      case "createPodMenu":
        await this.createPod(action.request);
        return;
      case "deletePod":
        await this.deleteActivePod();
        return;
      case "mergePod":
        await this.mergeActivePod();
        return;
      case "openPod":
        await this.openPodSelection(action.repository, action.pod);
        return;
      case "openPort":
        await this.openForwardedPort(false);
        return;
      case "openPortExternal":
        await this.openForwardedPort(true);
        return;
      case "openShell":
        this.openActiveShell();
        return;
      case "podMenu":
        await this.showPodSwitcher(action.request);
        return;
      case "refresh":
        await this.refresh(true);
        return;
      case "stopPod":
        await this.stopActivePod();
        return;
      case "viewDiff":
        await this.revealActiveReview();
        return;
      case "launchAgent":
        await this.launchAgent(action.agent);
        return;
    }
  }

  private async createPodSelection(
    repositoryRoot: string,
    agent: AgentKind,
    pod: string,
  ): Promise<void> {
    const validationError = validatePodName(pod);
    if (validationError !== undefined) {
      throw new Error(validationError);
    }
    const repository = await this.repositoryByRoot(repositoryRoot);
    const generation = ++this.selectionGeneration;
    const agents = [agent];
    await this.model.saveLaunchedAgents(repository, pod, agents);
    await this.model.rememberPod(repository, pod);
    if (generation !== this.selectionGeneration) {
      return;
    }
    this.active = {
      agents,
      generation,
      info: undefined,
      pod,
      repository,
    };
    this.updateStatus();
    await this.terminals.showActive(repository, pod, agents, this.model.executable());
    this.updateStatus();
    const selected = this.active;
    await this.enqueueReviewUpdate(async () => {
      if (this.currentSelection(selected) !== undefined) {
        await this.reviewDocuments.revealEmpty(repository, pod);
      }
    });
  }

  public async openPod(repository: Repository, pod: PodInfo): Promise<void> {
    const generation = ++this.selectionGeneration;
    const agents = this.agentsForPod(repository, pod);
    await this.model.saveLaunchedAgents(repository, pod.name, agents);
    await this.model.rememberPod(repository, pod.name);
    if (generation !== this.selectionGeneration) {
      return;
    }
    this.active = {
      agents,
      generation,
      info: pod,
      pod: pod.name,
      repository,
    };
    const selected = this.active;
    this.updateStatus();
    await this.terminals.showActive(repository, pod.name, agents, this.model.executable());
    this.updateStatus();

    try {
      await this.revealActiveReview();
    } catch (error) {
      this.model.logError(`opening review for ${pod.name}`, error);
      if (this.currentSelection(selected) === undefined) {
        return;
      }
      const message = errorMessage(error);
      try {
        await this.reviewDocuments.close(selected.repository, selected.pod);
      } catch (clearError) {
        this.model.logError(`clearing review for ${pod.name}`, clearError);
      }
      await vscode.window.showWarningMessage(
        `The ${pod.name} pod is open, but its review is not available: ${message}`,
      );
    }
    await this.tryAutomaticPortPreview(selected);
  }

  public async launchAgentMenu(request?: number): Promise<void> {
    const selected = this.active;
    if (selected === undefined) {
      throw new Error("select a pod before launching an agent");
    }
    const menuRequest = await this.terminals.beginMenu("agent", request);
    try {
      await this.terminals.showAgentMenu(
        this.agentOptions(undefined, selected.agents),
        menuRequest,
      );
    } catch (error) {
      this.terminals.dismissMenu(menuRequest);
      throw error;
    }
  }

  private async launchAgent(agent: AgentKind): Promise<void> {
    const selected = this.active;
    if (selected === undefined) {
      throw new Error("select a pod before launching an agent");
    }
    const result = this.agentLaunches.then(() => this.launchAgentNow(selected, agent));
    this.agentLaunches = result.catch(() => {});
    await result;
  }

  private async launchAgentNow(selected: ActivePod, agent: AgentKind): Promise<void> {
    let active = this.currentSelection(selected);
    if (active === undefined) {
      return;
    }
    if (!active.agents.includes(agent)) {
      const agents = [...active.agents, agent];
      await this.model.saveLaunchedAgents(active.repository, active.pod, agents);
      active = this.currentSelection(selected);
      if (active === undefined) {
        return;
      }
      this.active = { ...active, agents };
      active = this.active;
      this.updateStatus();
    }
    await this.terminals.launchAgent(
      active.repository,
      active.pod,
      agent,
      this.model.executable(),
    );
  }

  public openActiveShell(): void {
    const selected = this.active;
    if (selected === undefined) {
      throw new Error("select a pod before opening a shell");
    }
    this.terminals.showShell(
      selected.repository,
      selected.pod,
      this.model.executable(),
    );
  }

  public async openForwardedPort(external: boolean): Promise<void> {
    const selected = this.requireActivePod("opening a forwarded port from");
    if (selected.info?.status !== "Running") {
      throw new Error(`pod '${selected.pod}' is not running`);
    }
    const port = await this.pickForwardedPort(selected);
    if (port === undefined || this.currentSelection(selected) === undefined) {
      return;
    }
    if (external) {
      const resolved = await vscode.env.asExternalUri(localPortUri(port));
      if (this.currentSelection(selected) === undefined) {
        return;
      }
      const opened = await vscode.env.openExternal(resolved);
      if (!opened) {
        throw new Error(`the external browser did not open ${portDisplayName(port)}`);
      }
      return;
    }
    await this.openPortPreview(selected, port, false);
  }

  private async pickForwardedPort(selected: ActivePod): Promise<PortInfo | undefined> {
    const ports = await this.model.listPorts(selected.repository, selected.pod);
    if (this.currentSelection(selected) === undefined) {
      return undefined;
    }
    if (ports.length === 0) {
      void vscode.window.showInformationMessage(
        `Pod '${selected.pod}' does not expose any forwarded ports.`,
      );
      return undefined;
    }
    if (ports.length === 1) {
      return ports[0];
    }
    const picked = await vscode.window.showQuickPick<PortPick>(
      ports.map((port) => ({
        description: localPortUri(port).toString(true),
        detail: `Forwards ${portTarget(port)}`,
        label: portDisplayName(port),
        port,
      })),
      {
        placeHolder: `Select a forwarded port from ${selected.pod}`,
        title: "Open Forwarded Port",
      },
    );
    return picked?.port;
  }

  private async tryAutomaticPortPreview(selected: ActivePod): Promise<void> {
    if (
      selected.info?.status !== "Running" ||
      this.automaticPreviewAttemptGeneration === selected.generation ||
      this.automaticPreviewGeneration === selected.generation ||
      this.currentSelection(selected) === undefined
    ) {
      return;
    }
    this.automaticPreviewAttemptGeneration = selected.generation;
    try {
      await this.reviewUpdates;
      if (this.currentSelection(selected) === undefined) {
        return;
      }
      const ports = await this.model.listPorts(selected.repository, selected.pod);
      if (this.currentSelection(selected) === undefined) {
        return;
      }
      const previews = ports.filter((port) => port.on_auto_forward === "openPreview");
      const resolvedPreviews: { readonly port: PortInfo; readonly uri: vscode.Uri }[] = [];
      for (const preview of previews) {
        const uri = await vscode.env.asExternalUri(localPortUri(preview));
        if (this.currentSelection(selected) === undefined) {
          return;
        }
        resolvedPreviews.push({ port: preview, uri });
      }
      const reviewTab = this.reviewDocuments.activeReviewTab(
        selected.repository,
        selected.pod,
      );
      if (reviewTab === undefined) {
        // Do not risk replacing an editor the user chose while the pod started.
        return;
      }
      const previousTabs = new Set(
        vscode.window.tabGroups.all.flatMap((group) => group.tabs),
      );
      const previewTitles = new Set(
        resolvedPreviews.map(({ port }) => `${selected.pod}: ${portDisplayName(port)}`),
      );
      let unrelatedEditorActivated = false;
      const focusSubscription = vscode.window.tabGroups.onDidChangeTabs((event) => {
        if (
          event.closed.some((tab) =>
            this.reviewDocuments.isReviewTab(selected.repository, selected.pod, tab)
          )
        ) {
          unrelatedEditorActivated = true;
        }
        const active = [...event.opened, ...event.changed].find((tab) => tab.isActive);
        if (
          active !== undefined &&
          active !== reviewTab &&
          active.label !== reviewTab?.label &&
          !previewTitles.has(active.label)
        ) {
          unrelatedEditorActivated = true;
        }
      });
      const opened: vscode.WebviewPanel[] = [];
      try {
        for (const { port, uri } of resolvedPreviews) {
          const panel = this.portPreviews.open(
            selected.repository.root,
            selected.pod,
            portTarget(port),
            `${selected.pod}: ${portDisplayName(port)}`,
            uri,
            true,
          );
          if (panel !== undefined) {
            opened.push(panel);
          }
        }
        await settleOpenedPreviewTabs(opened, previousTabs);
        if (
          opened.some((panel) => panel.active) &&
          !unrelatedEditorActivated &&
          this.currentSelection(selected) !== undefined
        ) {
          // Web extension hosts can ignore preserveFocus while creating a panel.
          // Restore only the still-open review that creation displaced.
          await this.reviewDocuments.restoreFocus(
            selected.repository,
            selected.pod,
            reviewTab,
            () =>
              opened.some((panel) => panel.active) &&
              !unrelatedEditorActivated &&
              this.currentSelection(selected) !== undefined,
          );
        }
      } finally {
        focusSubscription.dispose();
      }
      this.automaticPreviewGeneration = selected.generation;
    } catch (error) {
      this.model.logError(`opening the automatic port preview for ${selected.pod}`, error);
      const message = errorMessage(error);
      void vscode.window.showWarningMessage(
        `The ${selected.pod} pod is open, but its port preview is not available: ${message}`,
      );
    } finally {
      if (this.automaticPreviewAttemptGeneration === selected.generation) {
        this.automaticPreviewAttemptGeneration = undefined;
      }
    }
  }

  private async openPortPreview(
    selected: ActivePod,
    port: PortInfo,
    preserveFocus: boolean,
  ): Promise<void> {
    const resolved = await vscode.env.asExternalUri(localPortUri(port));
    if (this.currentSelection(selected) === undefined) {
      return;
    }
    this.portPreviews.open(
      selected.repository.root,
      selected.pod,
      portTarget(port),
      `${selected.pod}: ${portDisplayName(port)}`,
      resolved,
      preserveFocus,
    );
  }

  public mergeActivePod(): Promise<void> {
    return this.enqueuePodOperation(async () => {
      const selected = this.requireActivePod("merging");
      const confirmation = await vscode.window.showWarningMessage(
        `Merge pod '${selected.pod}' into the current branch?`,
        {
          detail: "A successful merge updates this checkout and stops the pod.",
          modal: true,
        },
        "Merge Pod",
      );
      if (confirmation !== "Merge Pod" || this.currentSelection(selected) === undefined) {
        return;
      }
      await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: `Merging pod ${selected.pod}`,
        },
        () => this.model.mergePod(selected.repository, selected.pod),
      );
      await this.clearPodSelection(selected, false);
      void vscode.window.showInformationMessage(`Merged pod '${selected.pod}'.`);
    });
  }

  public stopActivePod(): Promise<void> {
    return this.enqueuePodOperation(async () => {
      const selected = this.requireActivePod("stopping");
      await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: `Stopping pod ${selected.pod}`,
        },
        () => this.model.stopPod(selected.repository, selected.pod),
      );
      await this.clearPodSelection(selected, false);
    });
  }

  public deleteActivePod(): Promise<void> {
    return this.enqueuePodOperation(async () => {
      const selected = this.requireActivePod("deleting");
      const confirmation = await vscode.window.showWarningMessage(
        `Permanently delete pod '${selected.pod}'?`,
        {
          detail: "The container and any unmerged work in this pod will be removed.",
          modal: true,
        },
        "Delete Pod",
      );
      if (confirmation !== "Delete Pod" || this.currentSelection(selected) === undefined) {
        return;
      }
      await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: `Deleting pod ${selected.pod}`,
        },
        () => this.model.deletePod(selected.repository, selected.pod),
      );
      await this.clearPodSelection(selected, true);
    });
  }

  public addSshKey(): Promise<void> {
    return this.enqueuePodOperation(async () => {
      const selected = this.requireActivePod("adding an SSH key to");
      const picked = await vscode.window.showOpenDialog({
        canSelectFiles: true,
        canSelectFolders: false,
        canSelectMany: false,
        defaultUri: vscode.Uri.file(path.join(os.homedir(), ".ssh")),
        openLabel: "Add SSH Key",
        title: `Add SSH key to ${selected.pod}`,
      });
      const key = picked?.[0];
      if (key === undefined || this.currentSelection(selected) === undefined) {
        return;
      }
      if (key.scheme !== "file") {
        throw new Error("select an SSH key from the extension host filesystem");
      }
      if (key.fsPath.endsWith(".pub")) {
        throw new Error("select the private SSH key, not its .pub file");
      }
      let passphrase: string | undefined;
      for (;;) {
        try {
          await vscode.window.withProgress(
            {
              location: vscode.ProgressLocation.Notification,
              title: `Adding SSH key to ${selected.pod}`,
            },
            () => this.model.addSshKey(
              selected.repository,
              selected.pod,
              key.fsPath,
              passphrase,
            ),
          );
          break;
        } catch (error) {
          if (!(error instanceof SshPassphraseRequiredError)) {
            throw error;
          }
          passphrase = await vscode.window.showInputBox({
            ignoreFocusOut: true,
            password: true,
            prompt: `Passphrase for ${path.basename(key.fsPath)}`,
            title: `Add SSH key to ${selected.pod}`,
            validateInput: (value) => value.length === 0 ? "Passphrase must not be empty" : undefined,
          });
          if (passphrase === undefined) {
            return;
          }
        }
      }
      void vscode.window.showInformationMessage(`Added SSH key to pod '${selected.pod}'.`);
    });
  }

  public async restoreLastPod(): Promise<boolean> {
    const generation = this.selectionGeneration;
    if (this.active !== undefined) {
      return false;
    }
    const last = this.model.lastPod();
    if (last === undefined) {
      return false;
    }
    const repositories = await this.model.repositories();
    const repository = repositories.find((candidate) => candidate.root === last.repository);
    if (repository === undefined) {
      return false;
    }
    const pods = await this.model.listPods(repository, false);
    const pod = pods.find((candidate) => candidate.name === last.pod);
    if (
      pod === undefined ||
      this.active !== undefined ||
      generation !== this.selectionGeneration
    ) {
      return false;
    }
    await this.openPod(repository, pod);
    return true;
  }

  public updateStatus(): void {
    const selected = this.active;
    if (selected === undefined) {
      this.status.text = "$(server-process) Rumpelpod";
      this.status.tooltip = "Select or create a rumpelpod";
      this.status.show();
      return;
    }
    const state = selected.info === undefined ? "starting" : podStatusLabel(selected.info.status);
    const repositoryState =
      selected.info?.repo_state === null || selected.info?.repo_state === undefined
        ? ""
        : ` / ${selected.info.repo_state}`;
    this.status.text = `$(comment-discussion) ${selected.pod} / ${state}${repositoryState}`;
    this.status.tooltip = statusTooltip(selected, state);
    this.status.show();
    this.terminals.updateActiveRepositoryState(
      selected.repository,
      selected.pod,
      selected.info?.repo_state ?? "",
    );
  }

  private currentSelection(selected: ActivePod): ActivePod | undefined {
    const active = this.active;
    if (
      active?.repository.root === selected.repository.root &&
      active.pod === selected.pod &&
      active.generation === selected.generation
    ) {
      return active;
    }
    return undefined;
  }

  private requireActivePod(operation: string): ActivePod {
    const selected = this.active;
    if (selected === undefined) {
      throw new Error(`select a pod before ${operation} it`);
    }
    return selected;
  }

  private async clearPodSelection(
    selected: ActivePod,
    forgetSessions: boolean,
  ): Promise<void> {
    const cleanup: Promise<unknown>[] = [
      forgetSessions
        ? this.model.forgetPod(selected.repository, selected.pod)
        : this.model.forgetRememberedPod(selected.repository, selected.pod),
      this.reviewDocuments.close(selected.repository, selected.pod),
    ];
    this.portPreviews.close(selected.repository.root, selected.pod);
    if (this.currentSelection(selected) !== undefined) {
      this.selectionGeneration += 1;
      this.active = undefined;
      this.updateStatus();
      cleanup.push(this.terminals.clearActive(selected.repository, selected.pod));
    }
    const failures = (await Promise.allSettled(cleanup))
      .filter((result): result is PromiseRejectedResult => result.status === "rejected")
      .map((result) => result.reason);
    if (failures.length === 1) {
      throw failures[0];
    }
    if (failures.length > 1) {
      throw new AggregateError(failures, "clearing the inactive pod UI failed");
    }
  }

  private enqueuePodOperation(operation: () => Promise<void>): Promise<void> {
    const result = this.podOperations.then(operation);
    this.podOperations = result.catch(() => {});
    return result;
  }

  private enqueueReviewUpdate(update: () => Promise<void>): Promise<void> {
    const result = this.reviewUpdates.then(update);
    this.reviewUpdates = result.catch(() => {});
    return result;
  }

  private enqueueStatusUpdate(update: () => Promise<void>): Promise<void> {
    const result = this.statusUpdates.then(update);
    this.statusUpdates = result.catch(() => {});
    return result;
  }

  private agentOptions(
    selected: AgentKind | undefined,
    launched: readonly AgentKind[],
  ): readonly AgentMenuOption[] {
    return AGENTS.map((agent) => ({
      agent,
      description: agentDescription(agent),
      label: agentLabel(agent),
      launched: launched.includes(agent),
      selected: agent === selected,
    }));
  }

  private async openPodSelection(repositoryRoot: string, podName: string): Promise<void> {
    const repository = await this.repositoryByRoot(repositoryRoot);
    const pods = await this.model.listPods(repository, false);
    const pod = pods.find((candidate) => candidate.name === podName);
    if (pod === undefined) {
      throw new Error(`pod '${podName}' is no longer available`);
    }
    await this.openPod(repository, pod);
  }

  private podPick(repository: Repository, pod: PodInfo, showRepository: boolean): PodMenuOption {
    const agents = this.agentsForPod(repository, pod);
    const agentStates = agents.map((agent) => {
      const activity = agentActivity(pod, agent);
      return activity === undefined
        ? agentLabel(agent)
        : `${agentLabel(agent)}: ${activity}`;
    });
    const descriptions = [
      showRepository ? repository.name : undefined,
      agentStates.join(", "),
      podStatusLabel(pod.status),
      pod.repo_state ?? undefined,
    ].filter((value): value is string => value !== undefined);
    return {
      active: this.isActivePod(repository.root, pod.name),
      detail: `Host: ${pod.host} - Created: ${pod.created}`,
      name: pod.name,
      repository: repository.root,
      status: podStatusLabel(pod.status),
      summary: descriptions.join(" - "),
    };
  }

  private async repositoryByRoot(root: string): Promise<Repository> {
    const repositories = await this.model.repositories();
    const repository = repositories.find((candidate) => candidate.root === root);
    if (repository === undefined) {
      throw new Error(`repository '${root}' is no longer open`);
    }
    return repository;
  }

  private agentsForPod(repository: Repository, pod: PodInfo): readonly AgentKind[] {
    const saved = this.model.launchedAgents(repository, pod.name) ?? [];
    const detected: AgentKind[] = [];
    if (pod.claude_state !== null) {
      detected.push("claude");
    }
    if (pod.codex_state !== null) {
      detected.push("codex");
    }
    const agents = [...new Set([...saved, ...detected])];
    return agents.length === 0 ? [this.model.defaultAgent()] : agents;
  }

}

function agentActivity(pod: PodInfo, agent: AgentKind): string | undefined {
  switch (agent) {
    case "claude":
      switch (pod.claude_state) {
        case null:
          return undefined;
        case "processing":
          return "processing";
        case "waiting_for_input":
          return "waiting";
        case "auth_error":
          return "authentication error";
        case "stopped":
          return "stopped";
      }
      throw new Error(`unknown Claude state: ${String(pod.claude_state)}`);
    case "codex":
      switch (pod.codex_state) {
        case null:
          return undefined;
        case "processing":
          return "processing";
        case "idle":
          return "idle";
        case "error":
          return "error";
      }
      throw new Error(`unknown Codex state: ${String(pod.codex_state)}`);
    case "grok":
    case "pi":
      return undefined;
  }
}

function podStatusLabel(status: PodInfo["status"]): string {
  switch (status) {
    case "Running":
      return "running";
    case "Stopped":
      return "stopped";
    case "Gone":
      return "gone";
    case "Disconnected":
      return "disconnected";
    case "Stopping":
      return "stopping";
    case "Deleting":
      return "deleting";
    case "Broken":
      return "broken";
  }
}

function statusTooltip(selected: ActivePod, state: string): vscode.MarkdownString {
  const tooltip = new vscode.MarkdownString(undefined, true);
  tooltip.appendMarkdown(`**${selected.pod}**\n\n`);
  tooltip.appendMarkdown(`Agents: ${selected.agents.map(agentLabel).join(", ")}\n\n`);
  tooltip.appendMarkdown(`State: ${state}\n\n`);
  if (selected.info?.repo_state !== null && selected.info?.repo_state !== undefined) {
    tooltip.appendMarkdown(`Repository: ${selected.info.repo_state}\n\n`);
  }
  tooltip.appendMarkdown("Select another pod or open a pod shell");
  return tooltip;
}

function validatePodName(value: string): string | undefined {
  if (value.length === 0) {
    return "Pod name must not be empty";
  }
  if (value.length > 63) {
    return "Pod name must be at most 63 characters";
  }
  if (!/^[a-z0-9](?:[-a-z0-9]*[a-z0-9])?$/.test(value)) {
    return "Use lower-case letters, numbers, and internal hyphens";
  }
  return undefined;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function localPortUri(port: PortInfo): vscode.Uri {
  return vscode.Uri.from({
    authority: `127.0.0.1:${port.local_port}`,
    scheme: port.protocol ?? "http",
  });
}

function portDisplayName(port: PortInfo): string {
  return port.label.length > 0 ? port.label : `Port ${portTarget(port)}`;
}

function portTarget(port: PortInfo): string {
  return port.service.length === 0
    ? String(port.container_port)
    : `${port.service}:${port.container_port}`;
}

async function settleOpenedPreviewTabs(
  panels: readonly vscode.WebviewPanel[],
  previousTabs: ReadonlySet<vscode.Tab>,
): Promise<void> {
  if (panels.length === 0) {
    return;
  }
  const published = (): boolean => {
    const remaining = panels.map((panel) => panel.title);
    for (const tab of vscode.window.tabGroups.all.flatMap((group) => group.tabs)) {
      if (previousTabs.has(tab)) {
        continue;
      }
      const index = remaining.indexOf(tab.label);
      if (index >= 0) {
        remaining.splice(index, 1);
      }
    }
    return remaining.length === 0;
  };

  await new Promise<void>((resolve) => {
    let settle: NodeJS.Timeout | undefined;
    const finish = (): void => {
      clearTimeout(timeout);
      if (settle !== undefined) {
        clearTimeout(settle);
      }
      subscription.dispose();
      resolve();
    };
    const check = (): void => {
      if (!published()) {
        return;
      }
      if (settle !== undefined) {
        clearTimeout(settle);
      }
      settle = setTimeout(finish, 250);
    };
    const timeout = setTimeout(finish, 2_000);
    const subscription = vscode.window.tabGroups.onDidChangeTabs(check);
    check();
  });
}
