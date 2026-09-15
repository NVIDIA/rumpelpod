// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Write};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};

// Archive construction uses synchronous filesystem APIs, but its output must
// be bounded and cancellation must release a producer blocked on the network.
pub(crate) struct StreamingUpload {
    task: tokio::task::JoinHandle<Result<()>>,
    cancel: watch::Sender<()>,
}

impl StreamingUpload {
    pub(crate) fn new(
        produce: impl FnOnce(&mut dyn Write) -> Result<()> + Send + 'static,
    ) -> (reqwest::Body, Self) {
        let (tx, rx) = mpsc::channel(2);
        let (cancel, cancelled) = watch::channel(());
        let task = tokio::task::spawn_blocking(move || {
            let mut writer = UploadWriter { tx, cancelled };
            let result = produce(&mut writer);
            if let Err(error) = &result {
                // The HTTP consumer must see a failed archive as a body error,
                // rather than accepting EOF as a successfully finished upload.
                if let Err(send_error) = writer.send(Err(io::Error::other(format!("{error:#}")))) {
                    log::debug!("upload consumer closed after producer failed: {send_error}");
                }
            }
            result
        });
        let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
        (body, Self { task, cancel })
    }

    pub(crate) async fn finish(self) -> Result<()> {
        // A server can reject a request without reading its body. Wake the
        // producer before joining so that response cannot strand a writer.
        drop(self.cancel);
        self.task.await.context("archive producer panicked")?
    }
}

struct UploadWriter {
    tx: mpsc::Sender<io::Result<Vec<u8>>>,
    cancelled: watch::Receiver<()>,
}

impl UploadWriter {
    fn send(&mut self, chunk: io::Result<Vec<u8>>) -> io::Result<()> {
        crate::async_runtime::block_on(async {
            tokio::select! {
                biased;
                _ = self.cancelled.changed() => {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "upload cancelled"))
                }
                result = self.tx.send(chunk) => {
                    result.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "upload closed"))
                }
            }
        })
    }
}

impl Write for UploadWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let len = bytes.len().min(64 * 1024);
        if len != 0 {
            self.send(Ok(bytes[..len].to_vec()))?;
        }
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
