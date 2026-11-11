const MAX_TEXT_LENGTH: usize = 4096;

struct HtmlTag {
    opening: String,
    closing: &'static str,
}

#[derive(Default)]
pub(crate) struct HtmlWriter {
    messages: Vec<String>,
    html: String,
    length: usize,
    has_content: bool,
    tags: Vec<HtmlTag>,
}

pub(crate) fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl HtmlWriter {
    pub(crate) fn text(&mut self, text: &str) {
        for character in text.chars() {
            if self.length + character.len_utf16() > MAX_TEXT_LENGTH {
                self.flush();
            }
            match character {
                '&' => self.html.push_str("&amp;"),
                '<' => self.html.push_str("&lt;"),
                '>' => self.html.push_str("&gt;"),
                '"' => self.html.push_str("&quot;"),
                _ => self.html.push(character),
            }
            self.length += character.len_utf16();
            self.has_content |= !character.is_whitespace();
        }
    }

    pub(crate) fn open(&mut self, opening: impl Into<String>, closing: &'static str) {
        if self.length == MAX_TEXT_LENGTH {
            self.flush();
        }
        let opening = opening.into();
        self.html.push_str(&opening);
        self.tags.push(HtmlTag { opening, closing });
    }

    pub(crate) fn close(&mut self) {
        if let Some(tag) = self.tags.pop() {
            self.html.push_str(tag.closing);
        }
    }

    pub(crate) fn code(&mut self, text: &str, block: bool) {
        let suspended = std::mem::take(&mut self.tags);
        for tag in suspended.iter().rev() {
            self.html.push_str(tag.closing);
        }
        if block {
            self.open("<pre>", "</pre>");
        } else {
            self.open("<code>", "</code>");
        }
        self.text(text);
        self.close();
        for tag in suspended {
            self.open(tag.opening, tag.closing);
        }
    }

    fn flush(&mut self) {
        if self.length == 0 {
            return;
        }
        for tag in self.tags.iter().rev() {
            self.html.push_str(tag.closing);
        }
        let html = std::mem::take(&mut self.html);
        if self.has_content {
            self.messages.push(html);
        }
        for tag in &self.tags {
            self.html.push_str(&tag.opening);
        }
        self.length = 0;
        self.has_content = false;
    }

    pub(crate) fn finish(mut self) -> Vec<String> {
        self.flush();
        self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::{HtmlWriter, MAX_TEXT_LENGTH, escape_html};

    #[test]
    fn escapes_text_and_link_attributes() {
        let mut writer = HtmlWriter::default();
        writer.text("<tag> & \"text\"");
        assert_eq!(writer.finish(), vec!["&lt;tag&gt; &amp; &quot;text&quot;"]);
        assert_eq!(escape_html("a\"&<b>"), "a&quot;&amp;&lt;b&gt;");
    }

    #[test]
    fn omits_empty_output_and_empty_formatting() {
        let mut writer = HtmlWriter::default();
        writer.open("<b>", "</b>");
        writer.close();
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn omits_whitespace_only_messages() {
        let mut writer = HtmlWriter::default();
        writer.text("  \n\t");
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn reopens_nested_tags_when_splitting_long_messages() {
        let mut writer = HtmlWriter::default();
        writer.open("<b>", "</b>");
        writer.open("<i>", "</i>");
        writer.text(&"a".repeat(MAX_TEXT_LENGTH + 1));
        writer.close();
        writer.close();
        let messages = writer.finish();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0],
            format!("<b><i>{}</i></b>", "a".repeat(MAX_TEXT_LENGTH))
        );
        assert_eq!(messages[1], "<b><i>a</i></b>");
    }

    #[test]
    fn never_splits_an_html_entity_or_unicode_character() {
        let mut writer = HtmlWriter::default();
        writer.text(&"&".repeat(MAX_TEXT_LENGTH - 1));
        writer.text("🦀");
        let messages = writer.finish();
        assert_eq!(
            messages,
            vec!["&amp;".repeat(MAX_TEXT_LENGTH - 1), "🦀".into()]
        );
    }

    #[test]
    fn suspends_other_formatting_around_code() {
        let mut writer = HtmlWriter::default();
        writer.open("<b>", "</b>");
        writer.text("Run ");
        writer.code("a < b", false);
        writer.text(" now");
        writer.close();
        assert_eq!(
            writer.finish(),
            vec!["<b>Run </b><code>a &lt; b</code><b> now</b>"]
        );
    }

    #[test]
    fn keeps_long_code_blocks_valid_in_every_message() {
        let mut writer = HtmlWriter::default();
        writer.code(&"中".repeat(MAX_TEXT_LENGTH + 1), true);
        let messages = writer.finish();
        assert_eq!(
            messages[0],
            format!("<pre>{}</pre>", "中".repeat(MAX_TEXT_LENGTH))
        );
        assert_eq!(messages[1], "<pre>中</pre>");
    }

    #[test]
    fn does_not_add_an_empty_message_at_the_limit() {
        let mut writer = HtmlWriter::default();
        writer.text(&"a".repeat(MAX_TEXT_LENGTH));
        writer.open("<b>", "</b>");
        writer.close();
        assert_eq!(writer.finish(), vec!["a".repeat(MAX_TEXT_LENGTH)]);
    }
}
