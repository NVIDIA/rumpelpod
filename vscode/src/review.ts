// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

import * as vscode from "vscode";

import type { ReviewFile, ReviewPlan } from "./generated/protocol";
import type { Repository } from "./model";
import { runProcess } from "./process";
import {
  moveActiveTabToEnd,
  moveActiveTabToPosition,
  waitForActiveTab,
} from "./tabs";

const REVIEW_SCHEME = "rumpelpod-review";
const BINARY_FILE_MESSAGE = "Binary file cannot be displayed by the text diff editor.\n";

interface ReviewDocumentDescriptor {
  readonly kind: "file";
  readonly exists: boolean;
  readonly path: string;
  readonly repository: string;
  readonly revision: string;
}

interface ReviewTab {
  readonly pod: string;
  readonly repository: string;
  readonly resources: readonly MultiDiffResource[];
  readonly source: string;
  readonly tab: vscode.Tab;
}

interface ReviewSnapshot {
  readonly pod: string;
  readonly repository: string;
  readonly resources: readonly MultiDiffResource[];
  readonly source: string;
}

interface ReviewRecord {
  readonly current: ReviewTab;
  readonly pending: ReviewSnapshot | undefined;
}

type ReviewPlacement =
  | { readonly kind: "end" }
  | { readonly kind: "position"; readonly position: number }
  | { readonly kind: "unchanged" };

interface MultiDiffResource {
  readonly modifiedUri: vscode.Uri;
  readonly originalUri: vscode.Uri;
}

interface MultiDiffTabInput {
  readonly modified: vscode.Uri;
  readonly original: vscode.Uri;
}

interface OpenMultiDiffEditorOptions {
  readonly multiDiffSourceUri: vscode.Uri;
  readonly resources: readonly MultiDiffResource[];
  readonly title: string;
}

export class ReviewDocuments implements vscode.TextDocumentContentProvider, vscode.Disposable {
  private readonly registration: vscode.Disposable;
  private readonly tabSubscription: vscode.Disposable;
  private readonly content = new Map<string, Promise<string>>();
  private readonly reviews = new Map<string, ReviewRecord>();
  private disposed = false;
  private operations = Promise.resolve();

  public constructor(
    private readonly reportError: (context: string, error: unknown) => void,
  ) {
    this.registration = vscode.workspace.registerTextDocumentContentProvider(REVIEW_SCHEME, this);
    this.tabSubscription = vscode.window.tabGroups.onDidChangeTabs((event) => {
      this.forgetClosedEmptyReviews(event.closed);
      void this.applyPendingReview().catch((error: unknown) => {
        this.reportError("refreshing the activated pod review", error);
      });
    });
  }

  public provideTextDocumentContent(uri: vscode.Uri): Promise<string> {
    const key = uri.toString();
    const existing = this.content.get(key);
    if (existing !== undefined) {
      return existing;
    }
    const loaded = this.load(uri);
    this.content.set(key, loaded);
    loaded.catch(() => this.content.delete(key));
    return loaded;
  }

  public async reveal(
    repository: Repository,
    pod: string,
    plan: ReviewPlan,
  ): Promise<void> {
    return this.enqueue(() => this.openNow(repository, pod, plan, false));
  }

  public async refresh(
    repository: Repository,
    pod: string,
    plan: ReviewPlan,
  ): Promise<void> {
    return this.enqueue(() => this.openNow(repository, pod, plan, true));
  }

  private async openNow(
    repository: Repository,
    pod: string,
    plan: ReviewPlan,
    refreshOnly: boolean,
  ): Promise<void> {
    const source = populatedReviewUri(repository, pod, plan);
    const snapshot = {
      pod,
      repository: repository.root,
      resources: plan.files.map((file) => reviewResource(repository, plan, file)),
      source: source.toString(),
    } satisfies ReviewSnapshot;
    await this.updateNow(snapshot, refreshOnly);
  }

  public async revealEmpty(
    repository: Repository,
    pod: string,
  ): Promise<void> {
    return this.enqueue(() => this.openEmptyNow(repository, pod, false));
  }

  public viewColumn(repository: Repository, pod: string): vscode.ViewColumn | undefined {
    const current = this.reviews.get(reviewKey(repository.root, pod))?.current;
    if (current !== undefined) {
      const open = this.findOpenTab(current);
      if (open !== undefined) {
        return open.group.viewColumn;
      }
    }
    return vscode.window.tabGroups.all
      .flatMap((group) => group.tabs)
      .find((tab) => isPodReviewTab(tab, repository.root, pod))
      ?.group.viewColumn;
  }

  public focus(repository: Repository, pod: string): Promise<void> {
    return this.enqueue(async () => {
      const key = reviewKey(repository.root, pod);
      const record = this.reviews.get(key);
      if (record === undefined) {
        return;
      }
      const tab = await this.show(record.current, { kind: "unchanged" });
      this.reviews.set(key, {
        current: { ...record.current, tab },
        pending: record.pending,
      });
    });
  }

  public async refreshEmpty(
    repository: Repository,
    pod: string,
  ): Promise<void> {
    return this.enqueue(() => this.openEmptyNow(repository, pod, true));
  }

  private async openEmptyNow(
    repository: Repository,
    pod: string,
    refreshOnly: boolean,
  ): Promise<void> {
    const source = emptyReviewUri(repository, pod);
    const snapshot = {
      pod,
      repository: repository.root,
      resources: [],
      source: source.toString(),
    } satisfies ReviewSnapshot;
    await this.updateNow(snapshot, refreshOnly);
  }

  private async updateNow(snapshot: ReviewSnapshot, refreshOnly: boolean): Promise<void> {
    const key = reviewKey(snapshot.repository, snapshot.pod);
    const record = this.reviews.get(key);
    let current = record?.current;
    let openTab = current === undefined ? undefined : this.findOpenTab(current);
    if (!refreshOnly && openTab === undefined) {
      const restoredTabs = this.findPodTabs(snapshot);
      const exact = restoredTabs.find((tab) => isSnapshotReviewTab(tab, snapshot));
      const staleTabs = restoredTabs.filter((tab) => tab !== exact);
      if (staleTabs.length > 0) {
        await vscode.window.tabGroups.close(staleTabs, true);
        for (const tab of staleTabs) {
          this.evictTab(tab);
        }
      }
      if (exact !== undefined) {
        current = { ...snapshot, tab: exact };
        openTab = exact;
        this.reviews.set(key, { current, pending: record?.pending });
      }
    }
    const currentIsOpen = openTab !== undefined;
    if (current !== undefined && openTab !== undefined && openTab !== current.tab) {
      current = { ...current, tab: openTab };
      this.reviews.set(key, { current, pending: record?.pending });
    }
    let placement: ReviewPlacement;
    if (current === undefined || openTab === undefined) {
      placement = { kind: "end" };
    } else if (current.source !== snapshot.source) {
      placement = { kind: "position", position: tabPosition(openTab) };
    } else {
      placement = { kind: "unchanged" };
    }
    if (refreshOnly) {
      if (current === undefined || !currentIsOpen) {
        return;
      }
      if (current.source === snapshot.source) {
        this.reviews.set(key, { current, pending: undefined });
        return;
      }
      if (!this.isActive(current)) {
        this.reviews.set(key, { current, pending: snapshot });
        return;
      }
    }

    if (current !== undefined && (current.source !== snapshot.source || !currentIsOpen)) {
      await this.closeReviewNow(key, current);
    }
    const tab = await this.show(snapshot, placement);
    if (this.disposed) {
      await this.closeDisposedTab(tab);
    }
    this.reviews.set(key, { current: { ...snapshot, tab }, pending: undefined });
  }

  private async show(
    snapshot: ReviewSnapshot,
    placement: ReviewPlacement,
  ): Promise<vscode.Tab> {
    // vscode.changes uses a random identity, which duplicates a restored review.
    await vscode.commands.executeCommand(
      "_workbench.openMultiDiffEditor",
      {
        multiDiffSourceUri: vscode.Uri.parse(snapshot.source),
        resources: snapshot.resources,
        title: snapshot.pod,
      } satisfies OpenMultiDiffEditorOptions,
    );
    let tab: vscode.Tab | undefined = snapshot.resources.length === 0
      ? vscode.window.tabGroups.activeTabGroup.activeTab
      : await waitForActiveTab(
        (candidate) => isSnapshotReviewTab(candidate, snapshot),
        `VS Code did not publish the review tab for pod '${snapshot.pod}'`,
      );
    // Editor commands act on the active editor, which another operation can
    // replace between awaits, so only change a tab proven to be this review.
    if (tab?.isPinned === true && isSnapshotReviewTab(tab, snapshot)) {
      tab = await changeReviewTab(
        "workbench.action.unpinEditor",
        snapshot,
        tab.group,
        (candidate) => !candidate.isPinned,
      );
    }
    if (tab?.isPreview === true && isSnapshotReviewTab(tab, snapshot)) {
      tab = await changeReviewTab(
        "workbench.action.keepEditor",
        snapshot,
        tab.group,
        (candidate) => !candidate.isPreview,
      );
    }
    if (
      tab === undefined ||
      tab.isPinned ||
      tab.isPreview ||
      !isSnapshotReviewTab(tab, snapshot)
    ) {
      throw new Error(`VS Code did not open the review for pod '${snapshot.pod}'`);
    }
    switch (placement.kind) {
      case "end":
        tab = await moveActiveTabToEnd((candidate) =>
          isSnapshotReviewTab(candidate, snapshot)
        );
        break;
      case "position":
        tab = await moveActiveTabToPosition(
          (candidate) => isSnapshotReviewTab(candidate, snapshot),
          placement.position,
        );
        break;
      case "unchanged":
        break;
    }
    return tab;
  }

  public async close(repository: Repository, pod: string): Promise<void> {
    return this.enqueue(async () => {
      const key = reviewKey(repository.root, pod);
      const current = this.reviews.get(key)?.current;
      if (current !== undefined) {
        await this.closeReviewNow(key, current);
      }
      // VS Code can restore a populated tab before this extension rebuilds its
      // in-memory record, so cleanup also identifies its review file URIs.
      const restored = vscode.window.tabGroups.all
        .flatMap((group) => group.tabs)
        .filter((tab) => isPodReviewTab(tab, repository.root, pod));
      if (restored.length > 0) {
        await vscode.window.tabGroups.close(restored, true);
        for (const tab of restored) {
          this.evictTab(tab);
        }
      }
    });
  }

  private async closeReviewNow(key: string, review: ReviewTab): Promise<void> {
    this.reviews.delete(key);
    if (review.resources.length === 0) {
      // Empty multi-diff tab inputs omit their source URI, so asking the
      // editor service for that source is the only exact lookup after reload.
      await vscode.commands.executeCommand(
        "_workbench.openMultiDiffEditor",
        {
          multiDiffSourceUri: vscode.Uri.parse(review.source),
          resources: review.resources,
          title: review.pod,
        } satisfies OpenMultiDiffEditorOptions,
      );
      const tab = vscode.window.tabGroups.activeTabGroup.activeTab;
      if (tab === undefined || !isSnapshotReviewTab(tab, review)) {
        throw new Error(`VS Code did not find the review for pod '${review.pod}'`);
      }
      await vscode.window.tabGroups.close(tab, true);
    } else {
      const tabs = vscode.window.tabGroups.all.flatMap((group) =>
        group.tabs.filter((tab) => isSameReviewTab(tab, review))
      );
      if (tabs.length > 0) {
        await vscode.window.tabGroups.close(tabs, true);
      }
    }
    this.evict(review);
  }

  public dispose(): void {
    this.disposed = true;
    this.reviews.clear();
    this.registration.dispose();
    this.tabSubscription.dispose();
    this.content.clear();
  }

  private async applyPendingReview(): Promise<void> {
    return this.enqueue(async () => {
      const active = vscode.window.tabGroups.activeTabGroup.activeTab;
      if (active === undefined) {
        return;
      }
      const selected = [...this.reviews.entries()].find(([, record]) =>
        isSameReviewTab(active, record.current)
      );
      if (selected === undefined) {
        return;
      }
      const [key, record] = selected;
      const pending = record.pending;
      if (pending === undefined) {
        return;
      }
      this.reviews.set(key, { current: record.current, pending: undefined });
      try {
        const position = tabPosition(active);
        await this.closeReviewNow(key, record.current);
        const tab = await this.show(pending, { kind: "position", position });
        if (this.disposed) {
          await this.closeDisposedTab(tab);
        }
        this.reviews.set(key, { current: { ...pending, tab }, pending: undefined });
      } catch (error) {
        if (!this.disposed && !this.reviews.has(key)) {
          this.reviews.set(key, { current: record.current, pending });
        }
        throw error;
      }
    });
  }

  private enqueue<T>(operation: () => Promise<T>): Promise<T> {
    const result = this.operations.then(() => {
      if (this.disposed) {
        throw new Error("Rumpelpod reviews have been disposed");
      }
      return operation();
    });
    this.operations = result.then(() => undefined, () => undefined);
    return result;
  }

  private async closeDisposedTab(tab: vscode.Tab): Promise<never> {
    await vscode.window.tabGroups.close(tab, true);
    throw new Error("Rumpelpod reviews were disposed while opening an editor");
  }

  private async load(uri: vscode.Uri): Promise<string> {
    const descriptor = decodeDescriptor(uri);
    if (!descriptor.exists) {
      return "";
    }
    const object = `${descriptor.revision}:${descriptor.path}`;
    const result = await runProcess(
      "git",
      ["show", object],
      descriptor.repository,
    );
    if (result.stdout.includes(0)) {
      return BINARY_FILE_MESSAGE;
    }
    return result.stdout.toString("utf8");
  }

  private evict(snapshot: ReviewSnapshot): void {
    for (const resource of snapshot.resources) {
      this.content.delete(resource.modifiedUri.toString());
      this.content.delete(resource.originalUri.toString());
    }
  }

  private evictTab(tab: vscode.Tab): void {
    const diffs = multiDiffs(tab.input) ?? [];
    for (const diff of diffs) {
      this.content.delete(diff.modified.toString());
      this.content.delete(diff.original.toString());
    }
  }

  private forgetClosedEmptyReviews(closedTabs: readonly vscode.Tab[]): void {
    for (const [key, record] of this.reviews) {
      if (
        record.current.resources.length === 0 &&
        closedTabs.includes(record.current.tab)
      ) {
        this.reviews.delete(key);
        this.evict(record.current);
      }
    }
  }

  private findOpenTab(review: ReviewTab): vscode.Tab | undefined {
    const tabs = vscode.window.tabGroups.all.flatMap((group) => group.tabs);
    return tabs.find((tab) => isSameReviewTab(tab, review));
  }

  private findPodTabs(snapshot: ReviewSnapshot): readonly vscode.Tab[] {
    return vscode.window.tabGroups.all
      .flatMap((group) => group.tabs)
      .filter((tab) => isPodReviewTab(tab, snapshot.repository, snapshot.pod));
  }

  private isActive(review: ReviewTab): boolean {
    const active = vscode.window.tabGroups.activeTabGroup.activeTab;
    return active !== undefined && isSameReviewTab(active, review);
  }
}

function isSameReviewTab(tab: vscode.Tab, review: ReviewTab): boolean {
  if (tab === review.tab) {
    return true;
  }
  if (review.resources.length === 0) {
    return false;
  }
  return isSnapshotReviewTab(tab, review);
}

async function changeReviewTab(
  command: string,
  snapshot: ReviewSnapshot,
  group: vscode.TabGroup,
  settled: (tab: vscode.Tab) => boolean,
): Promise<vscode.Tab | undefined> {
  await vscode.commands.executeCommand(command);
  if (snapshot.resources.length > 0) {
    return await waitForActiveTab(
      (candidate) => isSnapshotReviewTab(candidate, snapshot) && settled(candidate),
      `VS Code did not publish the updated review tab for pod '${snapshot.pod}'`,
    );
  }
  const changed = group.activeTab;
  return changed !== undefined &&
      isSnapshotReviewTab(changed, snapshot) &&
      settled(changed)
    ? changed
    : undefined;
}

function tabPosition(tab: vscode.Tab): number {
  const position = tab.group.tabs.indexOf(tab);
  if (position < 0) {
    throw new Error("VS Code omitted an open review from its editor group");
  }
  return position;
}

function isSnapshotReviewTab(tab: vscode.Tab, snapshot: ReviewSnapshot): boolean {
  if (!isPodTabLabel(tab.label, snapshot.pod)) {
    return false;
  }
  const diffs = multiDiffs(tab.input);
  if (diffs === undefined || diffs.length !== snapshot.resources.length) {
    return false;
  }
  if (diffs.length === 0) {
    return true;
  }
  const actual = diffs
    .map((diff) => diffIdentity(diff.original, diff.modified))
    .sort();
  const expected = snapshot.resources
    .map((resource) => diffIdentity(resource.originalUri, resource.modifiedUri))
    .sort();
  return actual.every((identity, index) => identity === expected[index]);
}

function isPodReviewTab(tab: vscode.Tab, repository: string, pod: string): boolean {
  if (!isPodTabLabel(tab.label, pod)) {
    return false;
  }
  const diffs = multiDiffs(tab.input);
  return diffs !== undefined && diffs.length > 0 && diffs.every((diff) =>
    reviewRepository(diff.original) === repository &&
    reviewRepository(diff.modified) === repository
  );
}

function isPodTabLabel(label: string, pod: string): boolean {
  return label === pod || label.startsWith(`${pod} (`);
}

function reviewKey(repository: string, pod: string): string {
  return JSON.stringify([repository, pod]);
}

function multiDiffs(input: unknown): readonly MultiDiffTabInput[] | undefined {
  if (
    typeof input !== "object" ||
    input === null ||
    !("textDiffs" in input) ||
    !Array.isArray(input.textDiffs) ||
    !input.textDiffs.every(isMultiDiffTabInput)
  ) {
    return undefined;
  }
  return input.textDiffs;
}

function isMultiDiffTabInput(value: unknown): value is MultiDiffTabInput {
  return typeof value === "object" &&
    value !== null &&
    "original" in value &&
    isUri(value.original) &&
    "modified" in value &&
    isUri(value.modified);
}

function isUri(value: unknown): value is vscode.Uri {
  return typeof value === "object" &&
    value !== null &&
    "scheme" in value &&
    typeof value.scheme === "string" &&
    "toString" in value &&
    typeof value.toString === "function";
}

function diffIdentity(original: vscode.Uri, modified: vscode.Uri): string {
  return JSON.stringify([original.toString(), modified.toString()]);
}

function reviewRepository(uri: vscode.Uri): string | undefined {
  if (uri.scheme !== REVIEW_SCHEME) {
    return undefined;
  }
  return decodeDescriptor(uri).repository;
}

function emptyReviewUri(repository: Repository, pod: string): vscode.Uri {
  return vscode.Uri.from({
    scheme: REVIEW_SCHEME,
    authority: "empty",
    path: `/${pod}`,
    query: encodeURIComponent(repository.root),
  });
}

function populatedReviewUri(
  repository: Repository,
  pod: string,
  plan: ReviewPlan,
): vscode.Uri {
  return vscode.Uri.from({
    scheme: REVIEW_SCHEME,
    authority: "review",
    path: `/${pod}`,
    query: encodeURIComponent(JSON.stringify({
      base: plan.base,
      repository: repository.root,
      target: plan.target,
    })),
  });
}

function reviewResource(
  repository: Repository,
  plan: ReviewPlan,
  file: ReviewFile,
): MultiDiffResource {
  return {
    modifiedUri: reviewUri(
      repository,
      plan.target,
      file.path,
      file.target_exists,
      "target",
    ),
    originalUri: reviewUri(
      repository,
      plan.base,
      file.path,
      file.base_exists,
      "base",
    ),
  };
}

function reviewUri(
  repository: Repository,
  revision: string,
  filePath: string,
  exists: boolean,
  side: "base" | "target",
): vscode.Uri {
  const descriptor: ReviewDocumentDescriptor = {
    exists,
    kind: "file",
    path: filePath,
    repository: repository.root,
    revision,
  };
  return vscode.Uri.from({
    scheme: REVIEW_SCHEME,
    authority: side,
    path: `/${filePath}`,
    query: encodeURIComponent(JSON.stringify(descriptor)),
  });
}

function decodeDescriptor(uri: vscode.Uri): ReviewDocumentDescriptor {
  let parsed: unknown;
  try {
    parsed = JSON.parse(decodeURIComponent(uri.query));
  } catch (error) {
    throw new Error(`invalid rumpelpod review URI: ${uri.toString()}`, { cause: error });
  }
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    !("kind" in parsed) ||
    typeof parsed.kind !== "string"
  ) {
    throw new Error(`invalid rumpelpod review descriptor: ${uri.toString()}`);
  }
  if (
    parsed.kind !== "file" ||
    !("exists" in parsed) ||
    !("path" in parsed) ||
    !("repository" in parsed) ||
    !("revision" in parsed) ||
    typeof parsed.exists !== "boolean" ||
    typeof parsed.path !== "string" ||
    typeof parsed.repository !== "string" ||
    typeof parsed.revision !== "string"
  ) {
    throw new Error(`invalid rumpelpod review descriptor: ${uri.toString()}`);
  }
  return {
    exists: parsed.exists,
    kind: "file",
    path: parsed.path,
    repository: parsed.repository,
    revision: parsed.revision,
  };
}
