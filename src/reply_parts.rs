//! Bounded Markdown delivery parts. The original reply remains in the outbox.
//! Prefer paragraph/line boundaries, keep ordinary links and inline code intact,
//! and close/reopen fenced code when a code block spans messages.
use anyhow::{Result, ensure};

pub const PART_CHARS: usize = 10_000;
#[derive(Debug)]
pub struct Part {
    pub source: String,
    pub markdown: String,
}

pub fn split(text: &str) -> Result<Vec<Part>> {
    ensure!(
        text.chars().count() <= crate::model::MAX_REPLY_CHARS,
        "Reply exceeds delivery limit"
    );
    let mut parts = Vec::new();
    let mut source = String::new();
    let mut rendered = String::new();
    let mut fence: Option<String> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let mark = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        // Keep the entire fence delimiter line together.
        let delimiter = mark.is_some()
            && (fence.is_none()
                || trimmed.starts_with(fence.as_ref().unwrap().chars().next().unwrap()));
        let mut rest = line;
        while !rest.is_empty() {
            let reserve = 160; // a reopened fence and optional closing newline
            let remaining = PART_CHARS.saturating_sub(rendered.chars().count() + reserve);
            if rest.chars().count() <= remaining {
                source.push_str(rest);
                rendered.push_str(rest);
                break;
            }
            if !source.is_empty() {
                flush(&mut parts, &mut source, &mut rendered, &fence);
                continue;
            }
            ensure!(
                !delimiter,
                "Markdown delimiter is too long to deliver safely"
            );
            let max = rest
                .char_indices()
                .nth(remaining)
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            let cut = if fence.is_some() {
                max
            } else {
                safe_break(&rest[..max]).ok_or_else(|| {
                    anyhow::anyhow!(
                        "A Markdown element exceeds the Slack message size; return it as a file"
                    )
                })?
            };
            ensure!(cut > 0, "Markdown element cannot be split safely");
            source.push_str(&rest[..cut]);
            rendered.push_str(&rest[..cut]);
            rest = &rest[cut..];
            flush(&mut parts, &mut source, &mut rendered, &fence);
        }
        if let Some(mark) = mark {
            if fence.as_ref().is_some_and(|f| f.starts_with(mark)) {
                fence = None;
            } else if fence.is_none() && line.chars().count() < 120 {
                fence = Some(line.trim_end().to_owned());
            }
        }
    }
    if !source.is_empty() {
        flush(&mut parts, &mut source, &mut rendered, &None);
    }
    ensure!(parts.len() <= 32, "Reply has too many parts");
    Ok(parts)
}
fn flush(
    parts: &mut Vec<Part>,
    source: &mut String,
    rendered: &mut String,
    fence: &Option<String>,
) {
    if let Some(f) = fence {
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        rendered.push_str(if f.starts_with('`') { "```" } else { "~~~" });
    }
    parts.push(Part {
        source: std::mem::take(source),
        markdown: std::mem::take(rendered),
    });
    if let Some(f) = fence {
        rendered.push_str(f);
        rendered.push('\n');
    }
}
// Whitespace outside inline code, Markdown link destinations, and emphasis is
// safe. Unreasonably large atomic elements fail visibly instead of corrupting a URL.
fn safe_break(s: &str) -> Option<usize> {
    let mut last = None;
    let mut brackets = 0usize;
    let mut parens = 0usize;
    let mut code = false;
    let mut escaped = false;
    let mut emphasis: Vec<String> = Vec::new();
    let chars: Vec<_> = s.char_indices().collect();
    let mut at = 0;
    while at < chars.len() {
        let (i, c) = chars[at];
        if escaped {
            escaped = false;
            at += 1;
            continue;
        }
        if c == '\\' {
            escaped = true;
            at += 1;
            continue;
        }
        if c == '`' {
            code = !code;
            at += 1;
            continue;
        }
        if code {
            at += 1;
            continue;
        }
        match c {
            '[' => brackets += 1,
            ']' => brackets = brackets.saturating_sub(1),
            '(' => parens += 1,
            ')' => parens = parens.saturating_sub(1),
            _ => {}
        }
        if brackets == 0 && parens == 0 && matches!(c, '*' | '_' | '~') {
            let word_underscore = c == '_'
                && at > 0
                && at + 1 < chars.len()
                && chars[at - 1].1.is_alphanumeric()
                && chars[at + 1].1.is_alphanumeric();
            if !word_underscore {
                let start = at;
                while at < chars.len() && chars[at].1 == c {
                    at += 1;
                }
                let count = at - start;
                if c != '~' || count >= 2 {
                    let marker = c.to_string().repeat(count);
                    if emphasis.last() == Some(&marker) {
                        emphasis.pop();
                    } else {
                        emphasis.push(marker);
                    }
                }
                continue;
            }
        }
        if c.is_whitespace() && brackets == 0 && parens == 0 && emphasis.is_empty() {
            last = Some(i + c.len_utf8());
        }
        at += 1;
    }
    last
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_markdown_and_fenced_code_remain_complete() {
        let text = format!(
            "# Result\n\n{}\n```rust\n{}\n```\n\n[Evidence](https://example.org/result)\n",
            "Paragraph café 🌟 with **bold** and `code`.\n\n".repeat(400),
            "let value = \"🌟\";\n".repeat(1500)
        );
        let parts = split(&text).unwrap();
        assert!(parts.len() > 2);
        assert_eq!(
            parts.iter().map(|p| p.source.as_str()).collect::<String>(),
            text
        );
        for p in parts {
            assert!(p.markdown.chars().count() <= PART_CHARS);
            assert_eq!(p.markdown.matches("```").count() % 2, 0);
        }
    }
    #[test]
    fn long_paragraph_does_not_split_inside_link_or_unicode() {
        let text =
            ("Words [long reference](https://example.org/a_(b)?q=x) `inline code` **strong words** *emphasis words* 🌟 ").repeat(400);
        let parts = split(&text).unwrap();
        assert_eq!(
            parts.iter().map(|p| p.source.as_str()).collect::<String>(),
            text
        );
        for p in parts {
            assert_eq!(
                p.markdown.matches('[').count(),
                p.markdown.matches(']').count()
            );
            assert_eq!(p.markdown.matches('`').count() % 2, 0);
        }
    }
    #[test]
    fn oversize_and_indivisible_elements_fail_instead_of_losing_text() {
        assert!(split(&"x".repeat(100_001)).is_err());
        assert!(
            split(&format!(
                "[link](https://example.org/{})",
                "x".repeat(15_000)
            ))
            .is_err()
        );
    }
}
