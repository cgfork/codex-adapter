use std::{error::Error, fmt};

use codebridge_core::AgentOutput;
use serde_json::Value;

#[derive(Debug)]
pub struct JsonlError {
    pub line: usize,
    source: serde_json::Error,
}

impl fmt::Display for JsonlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid Codex JSONL at line {}: {}",
            self.line, self.source
        )
    }
}

impl Error for JsonlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

pub fn parse_jsonl(stdout: &str) -> Result<Vec<AgentOutput>, JsonlError> {
    let mut output = Vec::new();
    for (index, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|source| JsonlError {
            line: index + 1,
            source,
        })?;
        if let Some(message) = parse_event(&event) {
            output.push(message);
        }
    }
    Ok(output)
}

pub(crate) fn parse_event(event: &Value) -> Option<AgentOutput> {
    match event["type"].as_str() {
        Some("item.completed") => return parse_item(&event["item"]),
        Some("error") => return error_message(event),
        Some("turn.failed") => return error_message(&event["error"]),
        _ => {}
    }

    match event["method"].as_str() {
        Some("item/completed") => parse_item(&event["params"]["item"]),
        Some("error") => error_message(&event["params"]["error"]),
        Some("turn/completed") if event["params"]["turn"]["status"] == "failed" => {
            error_message(&event["params"]["turn"]["error"])
        }
        _ => error_message(&event["error"]),
    }
}

fn error_message(error: &Value) -> Option<AgentOutput> {
    error["message"]
        .as_str()
        .map(|message| AgentOutput::Error(message.to_owned()))
}

fn parse_item(item: &Value) -> Option<AgentOutput> {
    match item["type"].as_str()? {
        "agent_message" | "agentMessage" => item["text"]
            .as_str()
            .map(|text| AgentOutput::Message(text.to_owned())),
        "plan" => item["text"]
            .as_str()
            .map(|text| AgentOutput::Plan(text.to_owned())),
        "command_execution" | "commandExecution" => Some(AgentOutput::Command {
            command: item["command"].as_str()?.to_owned(),
            output: item["aggregated_output"]
                .as_str()
                .or_else(|| item["aggregatedOutput"].as_str())
                .unwrap_or_default()
                .to_owned(),
            exit_code: item["exit_code"]
                .as_i64()
                .or_else(|| item["exitCode"].as_i64()),
        }),
        "file_change" | "fileChange" => Some(AgentOutput::FilesChanged(
            item["changes"]
                .as_array()?
                .iter()
                .filter_map(|change| change["path"].as_str().map(str::to_owned))
                .collect(),
        )),
        "error" => error_message(item),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentOutput, parse_jsonl};

    #[test]
    fn reads_exec_messages_and_decodes_embedded_newlines() {
        let jsonl = r##"{"type":"item.completed","item":{"type":"agent_message","text":"# Done\n\n**Built** successfully."}}"##;
        assert_eq!(
            parse_jsonl(jsonl).unwrap(),
            vec![AgentOutput::Message(
                "# Done\n\n**Built** successfully.".into()
            ),]
        );
    }

    #[test]
    fn reads_app_server_messages() {
        let jsonl = r#"{"method":"item/completed","params":{"threadId":"thread-1","item":{"type":"agentMessage","text":"Hello","phase":"final_answer"}}}"#;
        assert_eq!(
            parse_jsonl(jsonl).unwrap(),
            vec![AgentOutput::Message("Hello".into())]
        );
    }

    #[test]
    fn reads_command_results_from_both_transports() {
        let jsonl = r#"{"type":"item.completed","item":{"type":"command_execution","command":"cargo test","aggregated_output":"ok\n","exit_code":0}}
{"method":"item/completed","params":{"item":{"type":"commandExecution","command":"cargo test","aggregatedOutput":"ok\n","exitCode":0}}}"#;
        let expected = AgentOutput::Command {
            command: "cargo test".into(),
            output: "ok\n".into(),
            exit_code: Some(0),
        };
        assert_eq!(
            parse_jsonl(jsonl).unwrap(),
            vec![expected.clone(), expected]
        );
    }

    #[test]
    fn ignores_protocol_messages_and_deltas_without_duplicating_completed_text() {
        let jsonl = r#"{"type":"thread.started","thread_id":"thread-1"}
{"method":"item/agentMessage/delta","params":{"delta":"Hello"}}
{"type":"item.started","item":{"type":"agent_message","text":"Hello"}}
{"method":"item/completed","params":{"item":{"type":"agentMessage","text":"Hello world"}}}"#;
        assert_eq!(
            parse_jsonl(jsonl).unwrap(),
            vec![AgentOutput::Message("Hello world".into())]
        );
    }

    #[test]
    fn reports_invalid_json_with_its_original_line_number() {
        let error = parse_jsonl("{}\n\n{invalid}\n").unwrap_err();
        assert_eq!(error.line, 3);
        assert!(error.to_string().contains("line 3"));
    }

    #[test]
    fn accepts_blank_lines_crlf_and_a_final_record_without_a_newline() {
        let jsonl = "\r\n{\"type\":\"error\",\"message\":\"failed\"}\r\n\r\n{}";
        assert_eq!(
            parse_jsonl(jsonl).unwrap(),
            vec![AgentOutput::Error("failed".into())]
        );
    }

    #[test]
    fn reads_rpc_errors_and_failed_turns() {
        let jsonl = r#"{"id":1,"error":{"code":-1,"message":"RPC failure"}}
{"type":"turn.failed","error":{"message":"Turn failed"}}
{"method":"error","params":{"error":{"message":"Server failure"}}}
{"method":"turn/completed","params":{"turn":{"status":"failed","error":{"message":"App turn failed"}}}}"#;
        assert_eq!(
            parse_jsonl(jsonl).unwrap(),
            vec![
                AgentOutput::Error("RPC failure".into()),
                AgentOutput::Error("Turn failed".into()),
                AgentOutput::Error("Server failure".into()),
                AgentOutput::Error("App turn failed".into()),
            ]
        );
    }

    #[test]
    fn reads_plan_and_file_change_items() {
        let jsonl = r#"{"method":"item/completed","params":{"item":{"type":"plan","text":"1. Build"}}}
{"type":"item.completed","item":{"type":"file_change","changes":[{"path":"src/main.rs","kind":"update"}]}}"#;
        assert_eq!(
            parse_jsonl(jsonl).unwrap(),
            vec![
                AgentOutput::Plan("1. Build".into()),
                AgentOutput::FilesChanged(vec!["src/main.rs".into()]),
            ]
        );
    }
}
