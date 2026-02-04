use std::fmt::Write;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// Convert markdown to `Telegram`-compatible HTML.
///
/// Auto-closes open tags, making it safe for streaming partial markdown.
#[must_use]
pub fn to_telegram_html(md: &str) -> String {
    let options = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(md, options);

    let mut out = String::with_capacity(md.len());
    // Stack of closing tags for auto-close on truncated input.
    let mut close_stack: Vec<&str> = Vec::new();
    // Track list nesting for numbered lists.
    let mut list_index: Vec<Option<u64>> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => handle_start(tag, &mut out, &mut close_stack, &mut list_index),
            Event::End(tag_end) => handle_end(tag_end, &mut out, &mut close_stack, &mut list_index),
            Event::Text(text) => escape_html(&text, &mut out),
            Event::Code(code) => {
                out.push_str("<code>");
                escape_html(&code, &mut out);
                out.push_str("</code>");
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Rule => out.push_str("\n---\n"),
            Event::Html(html) | Event::InlineHtml(html) => out.push_str(&html),
            _ => {}
        }
    }

    // Auto-close remaining open tags (streaming safety).
    while let Some(tag) = close_stack.pop() {
        out.push_str(tag);
    }

    // Trim trailing whitespace.
    let trimmed = out.trim_end();
    trimmed.to_owned()
}

fn handle_start(
    tag: Tag<'_>,
    out: &mut String,
    close_stack: &mut Vec<&str>,
    list_index: &mut Vec<Option<u64>>,
) {
    match tag {
        Tag::Heading { .. } | Tag::Strong => {
            out.push_str("<b>");
            close_stack.push("</b>");
        }
        Tag::BlockQuote(_) => {
            out.push_str("<blockquote>");
            close_stack.push("</blockquote>");
        }
        Tag::CodeBlock(kind) => match kind {
            CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                out.push_str("<pre><code class=\"language-");
                escape_html(&lang, out);
                out.push_str("\">");
                close_stack.push("</code></pre>");
            }
            _ => {
                out.push_str("<pre>");
                close_stack.push("</pre>");
            }
        },
        Tag::List(start) => {
            list_index.push(start);
        }
        Tag::Item => {
            if let Some(idx) = list_index.last_mut() {
                if let Some(n) = idx {
                    let _ = write!(out, "{n}. ");
                    *n += 1;
                } else {
                    out.push_str("\u{2022} "); // bullet
                }
            }
        }
        Tag::Emphasis => {
            out.push_str("<i>");
            close_stack.push("</i>");
        }
        Tag::Strikethrough => {
            out.push_str("<s>");
            close_stack.push("</s>");
        }
        Tag::Link { dest_url, .. } => {
            out.push_str("<a href=\"");
            escape_html(&dest_url, out);
            out.push_str("\">");
            close_stack.push("</a>");
        }
        _ => {}
    }
}

fn handle_end(
    tag_end: TagEnd,
    out: &mut String,
    close_stack: &mut Vec<&str>,
    list_index: &mut Vec<Option<u64>>,
) {
    match tag_end {
        TagEnd::Paragraph => out.push_str("\n\n"),
        TagEnd::Heading(_) | TagEnd::CodeBlock => {
            pop_close(close_stack, out);
            out.push('\n');
        }
        TagEnd::BlockQuote(_)
        | TagEnd::Emphasis
        | TagEnd::Strong
        | TagEnd::Strikethrough
        | TagEnd::Link => {
            pop_close(close_stack, out);
        }
        TagEnd::List(_) => {
            list_index.pop();
        }
        TagEnd::Item => {
            out.push('\n');
        }
        _ => {}
    }
}

fn pop_close(stack: &mut Vec<&str>, out: &mut String) {
    if let Some(tag) = stack.pop() {
        out.push_str(tag);
    }
}

fn escape_html(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold() {
        assert_eq!(to_telegram_html("**hello**"), "<b>hello</b>");
    }

    #[test]
    fn italic() {
        assert_eq!(to_telegram_html("*hello*"), "<i>hello</i>");
    }

    #[test]
    fn inline_code() {
        assert_eq!(to_telegram_html("`code`"), "<code>code</code>");
    }

    #[test]
    fn code_block_with_lang() {
        let md = "```rust\nfn main() {}\n```";
        let html = to_telegram_html(md);
        assert_eq!(
            html,
            "<pre><code class=\"language-rust\">fn main() {}\n</code></pre>"
        );
    }

    #[test]
    fn code_block_no_lang() {
        let md = "```\nhello\n```";
        let html = to_telegram_html(md);
        assert_eq!(html, "<pre>hello\n</pre>");
    }

    #[test]
    fn strikethrough() {
        assert_eq!(to_telegram_html("~~gone~~"), "<s>gone</s>");
    }

    #[test]
    fn heading() {
        assert_eq!(to_telegram_html("# Title"), "<b>Title</b>");
    }

    #[test]
    fn link() {
        assert_eq!(
            to_telegram_html("[click](https://example.com)"),
            "<a href=\"https://example.com\">click</a>"
        );
    }

    #[test]
    fn blockquote() {
        let html = to_telegram_html("> quoted");
        assert!(html.starts_with("<blockquote>quoted"));
        assert!(html.ends_with("</blockquote>"));
    }

    #[test]
    fn unordered_list() {
        let md = "- one\n- two";
        let html = to_telegram_html(md);
        assert!(html.contains("\u{2022} one"));
        assert!(html.contains("\u{2022} two"));
    }

    #[test]
    fn ordered_list() {
        let md = "1. one\n2. two";
        let html = to_telegram_html(md);
        assert!(html.contains("1. one"));
        assert!(html.contains("2. two"));
    }

    #[test]
    fn html_escaping() {
        assert_eq!(to_telegram_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }

    #[test]
    fn nested_bold_italic() {
        assert_eq!(
            to_telegram_html("***bold italic***"),
            "<i><b>bold italic</b></i>"
        );
    }

    #[test]
    fn empty_input() {
        assert_eq!(to_telegram_html(""), "");
    }

    #[test]
    fn plain_text_passthrough() {
        assert_eq!(to_telegram_html("just text"), "just text");
    }

    #[test]
    fn streaming_unclosed_code_block() {
        // Simulates truncated streaming where the code block was never closed.
        let md = "```rust\nfn main() {";
        let html = to_telegram_html(md);
        assert!(html.contains("<pre><code class=\"language-rust\">"));
        assert!(html.ends_with("</code></pre>"));
    }

    #[test]
    fn streaming_unclosed_bold() {
        let md = "hello **world";
        let html = to_telegram_html(md);
        // pulldown-cmark may or may not emit Start(Strong) for unclosed bold,
        // but we should at least not panic and produce valid output.
        assert!(html.contains("hello"));
        assert!(html.contains("world"));
    }
}
