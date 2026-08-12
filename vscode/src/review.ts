// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

import * as vscode from "vscode";

import type { ReviewFile, ReviewPlan } from "./generated/protocol";
import type { Repository } from "./model";
import { runProcess } from "./process";

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
  readonly title: string;
}

interface ReviewSnapshot {
  readonly pod: string;
  readonly repository: string;
  readonly resources: readonly MultiDiffResource[];
  readonly source: string;
  readonly title: string;
}

interface ReviewRecord {
  readonly current: ReviewTab;
  readonly pending: ReviewSnapshot | undefined;
}

interface MultiDiffResource {
  readonly modifiedUri: vscode.Uri;
  readonly originalUri: vscode.Uri;
}

interface MultiDiffTabInput {
  readonly modified: vscode.Uri;
  readonly original: vscode.Uri;
}

type ChangesResource = readonly [vscode.Uri, vscode.Uri, vscode.Uri];

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
    this.tabSubscription = vscode.window.tabGroups.onDidChangeTabs(() => {
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
      title: reviewTitle(repository, pod),
    } satisfies ReviewSnapshot;
    await this.updateNow(snapshot, refreshOnly);
  }

  public async revealEmpty(
    repository: Repository,
    pod: string,
  ): Promise<void> {
    return this.enqueue(() => this.openEmptyNow(repository, pod, false));
  }

  public activeReviewTab(repository: Repository, pod: string): vscode.Tab | undefined {
    const key = reviewKey(repository.root, pod);
    const record = this.reviews.get(key);
    if (record === undefined) {
      return undefined;
    }
    const active = vscode.window.tabGroups.activeTabGroup.activeTab;
    if (active === undefined || !isSameReviewTab(active, record.current)) {
      return undefined;
    }
    if (active !== record.current.tab) {
      this.reviews.set(key, {
        current: { ...record.current, tab: active },
        pending: record.pending,
      });
    }
    return active;
  }

  public isReviewTab(repository: Repository, pod: string, tab: vscode.Tab): boolean {
    const record = this.reviews.get(reviewKey(repository.root, pod));
    return record !== undefined && isSameReviewTab(tab, record.current);
  }

  public restoreFocus(
    repository: Repository,
    pod: string,
    expectedTab: vscode.Tab,
    shouldRestore: () => boolean,
  ): Promise<void> {
    return this.enqueue(async () => {
      const key = reviewKey(repository.root, pod);
      const record = this.reviews.get(key);
      const tabs = vscode.window.tabGroups.all.flatMap((group) => group.tabs);
      // Remote hosts can replace Tab wrappers while publishing a webview even
      // though the review never closed, so its title and resources are the
      // stable identity after the caller has ruled out a close event.
      const openTab = record === undefined
        ? undefined
        : tabs.find((tab) => tab === expectedTab || isSnapshotReviewTab(tab, record.current));
      if (
        !shouldRestore() ||
        record === undefined ||
        openTab === undefined ||
        this.isActive(record.current)
      ) {
        return;
      }
      const current = openTab === record.current.tab
        ? record.current
        : { ...record.current, tab: openTab };
      const snapshot = record.pending ?? current;
      await this.closeReviewNow(key, current);
      const tab = await this.show(snapshot);
      if (this.disposed) {
        await this.closeDisposedTab(tab);
      }
      this.reviews.set(key, { current: { ...snapshot, tab }, pending: undefined });
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
      title: reviewTitle(repository, pod),
    } satisfies ReviewSnapshot;
    await this.updateNow(snapshot, refreshOnly);
  }

  private async updateNow(snapshot: ReviewSnapshot, refreshOnly: boolean): Promise<void> {
    const key = reviewKey(snapshot.repository, snapshot.pod);
    const record = this.reviews.get(key);
    let current = record?.current;
    const openTab = current === undefined ? undefined : this.findOpenTab(current);
    const currentIsOpen = openTab !== undefined;
    if (current !== undefined && openTab !== undefined && openTab !== current.tab) {
      current = { ...current, tab: openTab };
      this.reviews.set(key, { current, pending: record?.pending });
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

    if (
      !refreshOnly &&
      current !== undefined &&
      currentIsOpen &&
      current.source === snapshot.source &&
      this.isActive(current)
    ) {
      this.reviews.set(key, { current, pending: undefined });
      return;
    }
    if (current !== undefined) {
      await this.closeReviewNow(key, current);
    }
    const tab = await this.show(snapshot);
    if (this.disposed) {
      await this.closeDisposedTab(tab);
    }
    this.reviews.set(key, { current: { ...snapshot, tab }, pending: undefined });
  }

  private async show(snapshot: ReviewSnapshot): Promise<vscode.Tab> {
    const previousTabs = new Set(
      vscode.window.tabGroups.all.flatMap((group) => group.tabs),
    );
    const resources = snapshot.resources.map((resource) => [
      resource.modifiedUri,
      resource.originalUri,
      resource.modifiedUri,
    ] satisfies ChangesResource);
    await vscode.commands.executeCommand(
      "vscode.changes",
      snapshot.title,
      resources,
    );
    return await waitForReviewTab(snapshot, previousTabs);
  }

  public async close(repository: Repository, pod: string): Promise<void> {
    return this.enqueue(async () => {
      const key = reviewKey(repository.root, pod);
      const current = this.reviews.get(key)?.current;
      if (current !== undefined) {
        await this.closeReviewNow(key, current);
      }
    });
  }

  private async closeReviewNow(key: string, review: ReviewTab): Promise<void> {
    this.reviews.delete(key);
    const tab = this.findOpenTab(review);
    if (tab !== undefined) {
      await vscode.window.tabGroups.close(tab, true);
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
        await this.closeReviewNow(key, record.current);
        const tab = await this.show(pending);
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

  private findOpenTab(review: ReviewTab): vscode.Tab | undefined {
    const tabs = vscode.window.tabGroups.all.flatMap((group) => group.tabs);
    return tabs.find((tab) => isSameReviewTab(tab, review));
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
  return isSnapshotReviewTab(tab, review);
}

async function waitForReviewTab(
  snapshot: ReviewSnapshot,
  previousTabs: ReadonlySet<vscode.Tab>,
): Promise<vscode.Tab> {
  const find = (): vscode.Tab | undefined => {
    const active = vscode.window.tabGroups.activeTabGroup.activeTab;
    if (
      active !== undefined &&
      !previousTabs.has(active) &&
      isSnapshotReviewTab(active, snapshot)
    ) {
      return active;
    }
    return vscode.window.tabGroups.all
      .flatMap((group) => group.tabs)
      .find((tab) => !previousTabs.has(tab) && isSnapshotReviewTab(tab, snapshot));
  };
  const existing = find();
  if (existing !== undefined) {
    return existing;
  }

  return await new Promise<vscode.Tab>((resolve, reject) => {
    const timeout = setTimeout(() => {
      subscription.dispose();
      reject(new Error(`VS Code did not publish the review tab for pod '${snapshot.pod}'`));
    }, 5_000);
    const subscription = vscode.window.tabGroups.onDidChangeTabs(() => {
      const tab = find();
      if (tab === undefined) {
        return;
      }
      clearTimeout(timeout);
      subscription.dispose();
      resolve(tab);
    });
    const tab = find();
    if (tab !== undefined) {
      clearTimeout(timeout);
      subscription.dispose();
      resolve(tab);
    }
  });
}

function isSnapshotReviewTab(tab: vscode.Tab, snapshot: ReviewSnapshot): boolean {
  if (tab.label !== snapshot.title && !tab.label.startsWith(`${snapshot.title} (`)) {
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

function reviewKey(repository: string, pod: string): string {
  return JSON.stringify([repository, pod]);
}

function reviewTitle(repository: Repository, pod: string): string {
  return `${pod} (Review - ${repository.root})`;
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
