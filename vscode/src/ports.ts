// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

import * as vscode from "vscode";

interface PortPreview {
  readonly panel: vscode.WebviewPanel;
  readonly podKey: string;
}

export class PortPreviews implements vscode.Disposable {
  private readonly previews = new Map<string, PortPreview>();

  public open(
    repositoryRoot: string,
    pod: string,
    portKey: string,
    title: string,
    uri: vscode.Uri,
    preserveFocus: boolean,
  ): vscode.WebviewPanel | undefined {
    const key = JSON.stringify([repositoryRoot, pod, portKey]);
    const existing = this.previews.get(key);
    if (existing !== undefined) {
      if (preserveFocus) {
        return undefined;
      }
      existing.panel.title = title;
      existing.panel.webview.html = previewHtml(title, uri);
      existing.panel.reveal(undefined, preserveFocus);
      return undefined;
    }

    const panel = vscode.window.createWebviewPanel(
      "rumpelpod.portPreview",
      title,
      { preserveFocus, viewColumn: vscode.ViewColumn.Active },
      {
        enableForms: true,
        enableScripts: true,
        localResourceRoots: [],
      },
    );
    const podKey = JSON.stringify([repositoryRoot, pod]);
    this.previews.set(key, { panel, podKey });
    panel.onDidDispose(() => this.previews.delete(key));
    panel.webview.html = previewHtml(title, uri);
    return panel;
  }

  public close(repositoryRoot: string, pod: string): void {
    const podKey = JSON.stringify([repositoryRoot, pod]);
    for (const preview of [...this.previews.values()]) {
      if (preview.podKey === podKey) {
        preview.panel.dispose();
      }
    }
  }

  public dispose(): void {
    for (const preview of [...this.previews.values()]) {
      preview.panel.dispose();
    }
  }
}

function previewHtml(title: string, uri: vscode.Uri): string {
  if (uri.scheme !== "http" && uri.scheme !== "https") {
    throw new Error(`cannot preview a forwarded ${uri.scheme} URI`);
  }
  const frameUrl = new URL(uri.toString(true));
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; frame-src ${escapeHtml(frameUrl.origin)}; style-src 'unsafe-inline';">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>${escapeHtml(title)}</title>
  <style>
    html, body, iframe { width: 100%; height: 100%; }
    body { margin: 0; overflow: hidden; }
    iframe { border: 0; display: block; }
  </style>
</head>
<body>
  <!-- An opaque origin prevents the untrusted page from reaching its parent webview. -->
  <iframe sandbox="allow-forms allow-scripts" referrerpolicy="no-referrer" src="${escapeHtml(frameUrl.href)}" title="${escapeHtml(title)}"></iframe>
</body>
</html>`;
}

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}
