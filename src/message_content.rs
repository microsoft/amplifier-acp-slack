use serde_json::Value;

// Slack's text fallback can omit whole table blocks. Preserve the fallback
// verbatim and supplement only tables, not the duplicate rich-text prose.
pub(crate) fn text(message: &Value) -> String {
    let fallback = message["text"].as_str().unwrap_or("");
    let mut result = fallback.to_owned();
    for block in message["blocks"].as_array().into_iter().flatten() {
        append_table(&mut result, fallback, block);
    }
    for attachment in message["attachments"].as_array().into_iter().flatten() {
        append_table(&mut result, fallback, attachment);
        for block in attachment["blocks"].as_array().into_iter().flatten() {
            append_table(&mut result, fallback, block);
        }
    }
    result
}

fn append_table(result: &mut String, fallback: &str, block: &Value) {
    if block["type"] != "table" {
        return;
    }
    let rows = if let Some(rows) = block["rows"].as_array() {
        rows.iter()
            .map(|row| {
                if let Some(cells) = row.as_array() {
                    let cells: Vec<_> = cells
                        .iter()
                        .map(|cell| {
                            cell_text(cell, 0)
                                .replace('\\', "\\\\")
                                .replace('|', "\\|")
                                .replace('\r', "\\r")
                                .replace('\n', "\\n")
                        })
                        .collect();
                    format!("| {} |", cells.join(" | "))
                } else {
                    row.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        // An unrecognized shape is still source evidence, not an empty table.
        block.to_string()
    };
    if rows.is_empty() || fallback.contains(&rows) {
        return;
    }
    if !result.is_empty() {
        result.push_str("\n\n");
    }
    result.push_str("[Slack table]\n");
    result.push_str(&rows);
}

fn cell_text(value: &Value, depth: usize) -> String {
    if depth < 8 {
        match value["type"].as_str() {
            Some("text" | "raw_text") => {
                if let Some(text) = value["text"].as_str() {
                    return text.to_owned();
                }
            }
            Some("raw_number") => {
                if value["value"].is_number() {
                    return value["value"].to_string();
                }
            }
            Some("link") => {
                if let Some(url) = value["url"].as_str() {
                    return match value["text"].as_str().filter(|text| *text != url) {
                        Some(text) => format!("{text} ({url})"),
                        None => url.to_owned(),
                    };
                }
            }
            Some(
                kind @ ("rich_text"
                | "rich_text_section"
                | "rich_text_list"
                | "rich_text_preformatted"
                | "rich_text_quote"),
            ) => {
                if let Some(elements) = value["elements"].as_array() {
                    return elements
                        .iter()
                        .map(|element| cell_text(element, depth + 1))
                        .collect::<Vec<_>>()
                        .join(if matches!(kind, "rich_text" | "rich_text_list") {
                            "\n"
                        } else {
                            ""
                        });
                }
            }
            _ => {}
        }
    }
    // Unknown cell/inline kinds (including mentions) retain their literal
    // source data rather than losing a value or guessing an identity.
    value.to_string()
}
