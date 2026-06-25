// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the `rumpel pi` subcommand.
//!
//! Like the claude tests, these verify that the pi CLI running inside a
//! container can reach the Anthropic API via the LLM cache proxy routed
//! through the pod server and git HTTP server tunnel, producing
//! deterministic, offline-reproducible results.

mod common;
mod install;
mod smoke;
mod system_prompt;
mod tool_use;
