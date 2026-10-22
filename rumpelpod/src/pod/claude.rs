// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};

use super::types::{ClaudeHookEvent, ClaudeState, NotifyClaudeStateRequest};

#[derive(Default)]
pub(super) struct ClaudeSessions {
    sessions: HashMap<String, Session>,
}

struct Session {
    main: ClaudeState,
    subagents: HashSet<String>,
}

impl ClaudeSessions {
    pub fn update(&mut self, request: NotifyClaudeStateRequest) -> ClaudeState {
        let session = self.sessions.entry(request.session_id).or_insert(Session {
            main: ClaudeState::WaitingForInput,
            subagents: HashSet::new(),
        });
        // SessionEnd can race with a terminating child's final hook. Keep the
        // ended session until SessionStart so that late hooks cannot revive it.
        if session.main != ClaudeState::Stopped
            || matches!(request.event, ClaudeHookEvent::SessionStart)
        {
            match request.event {
                ClaudeHookEvent::SessionStart => {
                    session.main = ClaudeState::WaitingForInput;
                    session.subagents.clear();
                }
                ClaudeHookEvent::UserPromptSubmit => session.main = ClaudeState::Processing,
                ClaudeHookEvent::Stop => session.main = ClaudeState::WaitingForInput,
                ClaudeHookEvent::StopFailure => session.main = ClaudeState::AuthError,
                ClaudeHookEvent::SessionEnd => {
                    session.main = ClaudeState::Stopped;
                    session.subagents.clear();
                }
                ClaudeHookEvent::SubagentStart { agent_id } => {
                    session.subagents.insert(agent_id);
                }
                ClaudeHookEvent::SubagentStop { agent_id } => {
                    session.subagents.remove(&agent_id);
                }
            }
        }

        let mut result = ClaudeState::Stopped;
        for session in self.sessions.values() {
            if !session.subagents.is_empty() {
                return ClaudeState::Processing;
            }
            match session.main {
                ClaudeState::Processing => return ClaudeState::Processing,
                ClaudeState::AuthError => result = ClaudeState::AuthError,
                ClaudeState::WaitingForInput if result != ClaudeState::AuthError => {
                    result = ClaudeState::WaitingForInput;
                }
                ClaudeState::WaitingForInput | ClaudeState::Stopped => {}
            }
        }
        result
    }
}
