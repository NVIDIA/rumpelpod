// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use tokio::task::JoinHandle;

use crate::async_runtime::RUNTIME;

const FIRST_DELAY: Duration = Duration::from_secs(10);
const REPEAT_DELAY: Duration = Duration::from_secs(60);

/// Guard that emits a progress message when an operation takes too long.
///
/// Spawns a tokio task that, after an initial delay, sends `message`
/// through the channel and repeats every 60 seconds.  Also calls
/// `log::info!` on each emission so tests using `RUST_LOG=rumpelpod=info`
/// get timestamped output.  Logging verbosity does not affect the delay
/// because daemon diagnostics should not make routine CLI operations noisy.
///
/// The task is aborted when the guard is dropped.
pub struct SlowGuard {
    task: JoinHandle<()>,
}

impl SlowGuard {
    pub fn new(message: impl Into<String>, tx: tokio::sync::mpsc::Sender<String>) -> Self {
        let message = message.into();
        let task = RUNTIME.spawn(async move {
            tokio::time::sleep(FIRST_DELAY).await;
            loop {
                log::info!("{message}");
                if tx.send(message.clone()).await.is_err() {
                    return;
                }
                tokio::time::sleep(REPEAT_DELAY).await;
            }
        });

        Self { task }
    }
}

impl Drop for SlowGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}
