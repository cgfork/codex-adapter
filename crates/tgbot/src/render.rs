use codexbridge::{AgentOutput, JsonlError, parse_jsonl};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::text::{HtmlWriter, escape_html};

pub fn render_jsonl(stdout: &str) -> Result<Vec<String>, JsonlError> {
    let mut messages = Vec::new();
    for output in parse_jsonl(stdout)? {
        let mut writer = HtmlWriter::default();
        match output {
            AgentOutput::Message(markdown) => markdown_html(&mut writer, &markdown),
            AgentOutput::Plan(markdown) => {
                heading(&mut writer, "Plan");
                markdown_html(&mut writer, &markdown);
            }
            AgentOutput::Command {
                command,
                output,
                exit_code,
            } => {
                heading(&mut writer, "Command");
                writer.code(&command, true);
                if !output.is_empty() {
                    writer.text("\n");
                    writer.code(&output, true);
                }
                if let Some(exit_code) = exit_code {
                    writer.text(&format!("\nExit code: {exit_code}"));
                }
            }
            AgentOutput::FilesChanged(paths) => {
                heading(&mut writer, "Files changed");
                for path in paths {
                    writer.text("• ");
                    writer.code(&path, false);
                    writer.text("\n");
                }
            }
            AgentOutput::Error(message) => {
                heading(&mut writer, "Error");
                writer.text(&message);
            }
        }
        messages.extend(writer.finish());
    }
    Ok(messages)
}

fn heading(writer: &mut HtmlWriter, title: &str) {
    writer.open("<b>", "</b>");
    writer.text(title);
    writer.close();
    writer.text("\n");
}

fn markdown_html(writer: &mut HtmlWriter, markdown: &str) {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut lists = Vec::new();
    let mut links = Vec::new();
    let mut quote_depth = 0;
    let mut code_block = None::<String>;

    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code_block = Some(String::new()),
            Event::End(TagEnd::CodeBlock) => {
                writer.code(&code_block.take().unwrap_or_default(), true);
                writer.text("\n\n");
            }
            Event::Text(text) if code_block.is_some() => {
                if let Some(code) = code_block.as_mut() {
                    code.push_str(&text);
                }
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => writer.text(&text),
            Event::Code(code) => writer.code(&code, false),
            Event::Start(Tag::Heading { .. }) | Event::Start(Tag::Strong) => {
                writer.open("<b>", "</b>");
            }
            Event::Start(Tag::Emphasis) => writer.open("<i>", "</i>"),
            Event::Start(Tag::Strikethrough) => writer.open("<s>", "</s>"),
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => writer.close(),
            Event::End(TagEnd::Heading(_)) => {
                writer.close();
                writer.text("\n\n");
            }
            Event::End(TagEnd::Paragraph) => writer.text("\n\n"),
            Event::Start(Tag::List(start)) => lists.push(start),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                writer.text("\n");
            }
            Event::Start(Tag::Item) => {
                writer.text(&"  ".repeat(lists.len().saturating_sub(1)));
                match lists.last_mut() {
                    Some(Some(number)) => {
                        writer.text(&format!("{number}. "));
                        *number += 1;
                    }
                    _ => writer.text("• "),
                }
            }
            Event::End(TagEnd::Item) => writer.text("\n"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                let allowed = dest_url.split_once(':').is_some_and(|(scheme, _)| {
                    matches!(
                        scheme.to_ascii_lowercase().as_str(),
                        "http" | "https" | "mailto" | "tg"
                    )
                });
                links.push(allowed);
                if allowed {
                    writer.open(format!("<a href=\"{}\">", escape_html(&dest_url)), "</a>");
                }
            }
            Event::End(TagEnd::Link) => {
                if links.pop() == Some(true) {
                    writer.close();
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                quote_depth += 1;
                if quote_depth == 1 {
                    writer.open("<blockquote>", "</blockquote>");
                } else {
                    writer.text("› ");
                }
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if quote_depth == 1 {
                    writer.close();
                    writer.text("\n\n");
                }
                quote_depth -= 1;
            }
            Event::SoftBreak | Event::HardBreak => writer.text("\n"),
            Event::Rule => writer.text("\n———\n"),
            Event::TaskListMarker(done) => writer.text(if done { "☑ " } else { "☐ " }),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::render_jsonl;

    fn exec_message(text: &str) -> String {
        json!({ "type": "item.completed", "item": { "type": "agent_message", "text": text } })
            .to_string()
    }

    #[test]
    fn renders_agent_markdown_with_telegram_supported_tags() {
        let jsonl = exec_message(
            "# Result\n\n**Bold**, *italic*, ~~old~~, `code`.\n\n[Docs](https://example.com?a=1&b=2)\n\n- item\n\n```rust\nlet value = 1 < 2;\n```\n",
        );
        let html = render_jsonl(&jsonl).unwrap().concat();
        for expected in [
            "<b>Result</b>",
            "<b>Bold</b>",
            "<i>italic</i>",
            "<s>old</s>",
            "<code>code</code>",
            "<a href=\"https://example.com?a=1&amp;b=2\">Docs</a>",
            "• item",
            "<pre>let value = 1 &lt; 2;\n</pre>",
        ] {
            assert!(html.contains(expected), "missing {expected} in {html}");
        }
        assert!(!html.contains("item.completed"));
        assert!(!html.contains("<p>"));
    }

    #[test]
    fn produces_the_same_rich_text_for_both_codex_transports() {
        let text = "**Done**\n\n`cargo test`";
        let app = json!({ "method": "item/completed", "params": { "item": { "type": "agentMessage", "text": text } } });
        assert_eq!(
            render_jsonl(&exec_message(text)).unwrap(),
            render_jsonl(&app.to_string()).unwrap()
        );
    }

    #[test]
    fn escapes_raw_html_and_ignores_unsafe_link_schemes() {
        let html = render_jsonl(&exec_message(
            "<script>alert(1)</script>\n\n[open](javascript:alert)",
        ))
        .unwrap()
        .concat();
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("href="));
        assert!(html.contains("open"));
    }

    #[test]
    fn keeps_long_markdown_formatted_across_message_boundaries() {
        let html = render_jsonl(&exec_message(&format!("**{}**", "🦀".repeat(3000)))).unwrap();
        assert_eq!(html.len(), 2);
        assert!(
            html.iter()
                .all(|part| part.starts_with("<b>") && part.contains("</b>"))
        );
    }

    #[test]
    fn omits_trailing_paragraph_separators_after_an_exact_length_message() {
        let html = render_jsonl(&exec_message(&format!("**{}**", "a".repeat(4096)))).unwrap();
        assert_eq!(html, vec![format!("<b>{}</b>", "a".repeat(4096))]);
    }

    #[test]
    fn renders_command_output_as_literal_code() {
        let jsonl = json!({ "type": "item.completed", "item": { "type": "command_execution", "command": "echo '<ok>'", "aggregated_output": "**literal** & <ok>", "exit_code": 0 } });
        let html = render_jsonl(&jsonl.to_string()).unwrap().concat();
        assert!(html.starts_with("<b>Command</b>"));
        assert!(html.contains("<pre>**literal** &amp; &lt;ok&gt;</pre>"));
        assert!(html.contains("Exit code: 0"));
    }

    #[test]
    fn rejects_malformed_json_and_skips_protocol_events() {
        assert!(render_jsonl("{invalid}").is_err());
        assert!(
            render_jsonl("{\"type\":\"turn.started\"}\n")
                .unwrap()
                .is_empty()
        );
    }
}
