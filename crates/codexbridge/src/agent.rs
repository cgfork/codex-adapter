use std::{
    collections::{HashSet, VecDeque},
    future::Future,
};

use codebridge_core::{
    AgentEvent, AgentOutput, ApprovalDecision, CodeAgent, ExecutionOutput, TurnStatus,
};
use serde_json::{Value, json};

use crate::{AppServer, AppServerError, BridgeResult, Codex, output::parse_event};

/// An initialized Codex connection adapted to the provider-neutral bridge.
pub struct CodexAgent {
    codex: Codex,
    server: AppServer,
    pending: VecDeque<AgentEvent>,
    loaded_sessions: HashSet<String>,
}

impl CodexAgent {
    pub fn new(codex: Codex, server: AppServer) -> Self {
        Self {
            codex,
            server,
            pending: VecDeque::new(),
            loaded_sessions: HashSet::new(),
        }
    }

    fn translate(event: Value, pending: &mut VecDeque<AgentEvent>) {
        let Some(method) = event["method"].as_str() else {
            return;
        };
        let params = &event["params"];
        let session_id = params["threadId"].as_str().map(str::to_owned);
        let turn_id = params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str())
            .map(str::to_owned);

        if let Some(id) = event.get("id") {
            let request_id = id.to_string();
            let request = match method {
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                    AgentEvent::ApprovalRequested {
                        request_id,
                        session_id,
                        turn_id,
                        description: approval_description(method, params),
                    }
                }
                _ => AgentEvent::UnsupportedRequest {
                    request_id,
                    session_id,
                    description: method.to_owned(),
                },
            };
            pending.push_back(request);
            return;
        }

        if method == "serverRequest/resolved" {
            if let (Some(session_id), Some(request_id)) = (session_id, params.get("requestId")) {
                pending.push_back(AgentEvent::RequestResolved {
                    session_id,
                    request_id: request_id.to_string(),
                });
            }
            return;
        }

        if !matches!(method, "item/completed" | "error" | "turn/completed") {
            return;
        }
        let Some(session_id) = session_id else {
            return;
        };
        if method != "turn/completed" {
            if let Some(output) = parse_event(&event) {
                pending.push_back(AgentEvent::Output {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    output: vec![output],
                });
            }
        }
        if method == "turn/completed" {
            let Some(turn_id) = turn_id else {
                return;
            };
            let status = match params["turn"]["status"].as_str() {
                Some("completed") => TurnStatus::Completed,
                Some("interrupted") => TurnStatus::Interrupted,
                _ => TurnStatus::Failed,
            };
            pending.push_back(AgentEvent::TurnCompleted {
                session_id,
                turn_id,
                status,
                error: params["turn"]["error"]["message"]
                    .as_str()
                    .map(str::to_owned),
            });
        }
    }
}

impl CodeAgent for CodexAgent {
    async fn next_event(&mut self) -> BridgeResult<Option<AgentEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let Some(event) = self.server.next_event().await? else {
                return Ok(None);
            };
            Self::translate(event, &mut self.pending);
        }
    }

    async fn start_session(&mut self) -> BridgeResult<String> {
        let session_id = self.server.start_thread(&self.codex).await?;
        self.loaded_sessions.insert(session_id.clone());
        Ok(session_id)
    }

    async fn start_turn(&mut self, session_id: &str, prompt: &str) -> BridgeResult<String> {
        if !self.loaded_sessions.contains(session_id) {
            let response = self
                .server
                .request(
                    "thread/resume",
                    json!({ "threadId": session_id, "excludeTurns": true }),
                )
                .await?;
            if response["thread"]["id"].as_str() != Some(session_id) {
                return Err(AppServerError::InvalidResponse {
                    method: "thread/resume".to_owned(),
                    message: "response has no matching thread ID",
                }
                .into());
            }
            self.loaded_sessions.insert(session_id.to_owned());
        }
        self.server.start_turn(session_id, prompt).await
    }

    async fn interrupt(&mut self, session_id: &str, turn_id: &str) -> BridgeResult<()> {
        self.server.interrupt(session_id, turn_id).await
    }

    async fn respond_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> BridgeResult<()> {
        let id: Value = serde_json::from_str(request_id)?;
        let decision = match decision {
            ApprovalDecision::Allow => "accept",
            ApprovalDecision::Deny => "decline",
        };
        self.server
            .respond(id, json!({ "decision": decision }))
            .await
    }

    async fn reject_request(&mut self, request_id: &str) -> BridgeResult<()> {
        let id: Value = serde_json::from_str(request_id)?;
        self.server.reject_request(id).await
    }

    fn exec(
        &self,
        prompt: String,
    ) -> impl Future<Output = BridgeResult<ExecutionOutput>> + Send + 'static {
        let codex = self.codex.clone();
        async move {
            let result = codex.exec(&prompt).await?;
            let output = crate::parse_jsonl(&result.stdout)
                .unwrap_or_else(|error| vec![AgentOutput::Error(error.to_string())]);
            Ok(ExecutionOutput {
                output,
                success: result.status.success(),
                status: result.status.to_string(),
                stderr: result.stderr,
            })
        }
    }

    async fn shutdown(&mut self) -> BridgeResult<()> {
        self.server.shutdown().await
    }

    fn is_connection_fatal(&self, error: &codebridge_core::BridgeError) -> bool {
        error
            .downcast_ref::<AppServerError>()
            .is_none_or(AppServerError::is_connection_fatal)
    }
}

fn approval_description(method: &str, params: &Value) -> String {
    let kind = if method == "item/commandExecution/requestApproval" {
        "command execution"
    } else {
        "file changes"
    };
    let reason = params["reason"]
        .as_str()
        .unwrap_or("Codex requires your approval.");
    let command = params["command"].as_str().unwrap_or_default();
    let cwd = params["cwd"].as_str().unwrap_or_default();
    let grant_root = params["grantRoot"].as_str().unwrap_or_default();
    let text = format!(
        "Approve {kind}?\n{reason}\n{command}\n{cwd}\n{grant_root}\nOnly the user who started this turn can decide."
    );
    text.chars().take(1800).collect()
}

#[cfg(test)]
mod tests {
    use codebridge_core::{AgentEvent, AgentOutput, TurnStatus};
    use serde_json::json;

    use super::CodexAgent;

    fn translate(event: serde_json::Value) -> Vec<AgentEvent> {
        let mut pending = std::collections::VecDeque::new();
        CodexAgent::translate(event, &mut pending);
        pending.into()
    }

    #[test]
    fn translates_completed_items_and_failed_turns_in_order() {
        assert_eq!(
            translate(
                json!({"method":"item/completed","params":{"threadId":"s","turnId":"t","item":{"type":"agentMessage","text":"hello"}}})
            ),
            vec![AgentEvent::Output {
                session_id: "s".into(),
                turn_id: Some("t".into()),
                output: vec![AgentOutput::Message("hello".into())]
            }]
        );
        assert_eq!(
            translate(
                json!({"method":"turn/completed","params":{"threadId":"s","turn":{"id":"t","status":"failed","error":{"message":"oops"}}}})
            ),
            vec![AgentEvent::TurnCompleted {
                session_id: "s".into(),
                turn_id: "t".into(),
                status: TurnStatus::Failed,
                error: Some("oops".into())
            }]
        );
        assert_eq!(
            translate(
                json!({"method":"error","params":{"threadId":"s","turnId":"t","error":{"message":"standalone"}}})
            ),
            vec![AgentEvent::Output {
                session_id: "s".into(),
                turn_id: Some("t".into()),
                output: vec![AgentOutput::Error("standalone".into())]
            }]
        );
        assert!(translate(json!({"method":"item/agentMessage/delta","params":{"threadId":"s","turnId":"t","delta":"partial"}})).is_empty());
    }

    #[test]
    fn approval_and_resolution_ids_preserve_json_types() {
        for id in [json!(42), json!("42")] {
            let request = translate(
                json!({"id":id,"method":"item/fileChange/requestApproval","params":{"threadId":"s","turnId":"t"}}),
            );
            assert!(
                matches!(&request[0], AgentEvent::ApprovalRequested { request_id, session_id: Some(session_id), turn_id: Some(turn_id), description } if request_id == &id.to_string() && session_id == "s" && turn_id == "t" && description.starts_with("Approve file changes?"))
            );
            assert_eq!(
                translate(
                    json!({"method":"serverRequest/resolved","params":{"threadId":"s","requestId":id}})
                ),
                vec![AgentEvent::RequestResolved {
                    session_id: "s".into(),
                    request_id: id.to_string()
                }]
            );
        }
        assert!(
            matches!(&translate(json!({"id":1,"method":"unknown","params":{"threadId":"s"}}))[0], AgentEvent::UnsupportedRequest { description, .. } if description == "unknown")
        );
    }
}
