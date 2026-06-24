// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{env, process::ExitCode};

fn main() -> ExitCode {
    match rumpelpod::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:?}");
            let has_backtrace = env::var("RUST_BACKTRACE").as_ref().map(|s| s.as_str()) == Ok("1");
            if has_backtrace {
                let bt = e.backtrace();
                eprintln!("backtrace:\n{bt}");
            }

            ExitCode::FAILURE
        }
    }
}
