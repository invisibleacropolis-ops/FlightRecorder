use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParsedAction {
    pub action: String,
    pub public_payload: Value,
    pub sensitive_text: Option<String>,
}

fn decode_js_string(literal: &str) -> Option<String> {
    if literal.starts_with('"') {
        return serde_json::from_str(literal).ok();
    }
    if !literal.starts_with('\'') || !literal.ends_with('\'') || literal.len() < 2 {
        return None;
    }

    let mut decoded = String::new();
    let mut chars = literal[1..literal.len() - 1].chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }
        let escaped = chars.next()?;
        match escaped {
            '\\' => decoded.push('\\'),
            '\'' => decoded.push('\''),
            '"' => decoded.push('"'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            'b' => decoded.push('\u{0008}'),
            'f' => decoded.push('\u{000c}'),
            other => decoded.push(other),
        }
    }
    Some(decoded)
}

fn extract_type_text(raw: &str, parsed: &Value) -> Option<String> {
    if let Some(text) = parsed.get("text").and_then(Value::as_str) {
        return Some(text.to_owned());
    }

    let text_re = Regex::new(
        r#"(?s)(?:^|[,{}]\s*)(?:"text"|'text'|text)\s*:\s*("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')"#,
    )
    .expect("static regex");
    text_re
        .captures(raw)
        .and_then(|captures| captures.get(1))
        .and_then(|value| decode_js_string(value.as_str()))
}

pub fn parse_sky_actions(tool_input: &Value) -> Vec<ParsedAction> {
    let Some(code) = tool_input.get("code").and_then(Value::as_str) else {
        return Vec::new();
    };

    let call_re = Regex::new(
        r#"(?s)(?:\bsky|\b[a-zA-Z_$][\w$]*\.sky)\s*\.\s*(get_screenshot|click|drag|move|press_key|scroll|type_text)\s*\((\{.*?\})?\s*\)"#,
    )
    .expect("static regex");

    call_re
        .captures_iter(code)
        .map(|caps| {
            let action = caps
                .get(1)
                .map(|m| m.as_str())
                .unwrap_or("unknown")
                .to_owned();
            let raw = caps.get(2).map(|m| m.as_str()).unwrap_or("{}");
            let parsed =
                serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!({ "dynamic": true }));
            if action == "type_text" {
                let sensitive_text = extract_type_text(raw, &parsed);
                let text_len = sensitive_text
                    .as_deref()
                    .map(str::chars)
                    .map(Iterator::count);
                ParsedAction {
                    action,
                    public_payload: json!({ "text_length": text_len, "redacted": true }),
                    sensitive_text,
                }
            } else {
                ParsedAction {
                    action,
                    public_payload: parsed,
                    sensitive_text: None,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_real_sky_calls_and_redacts_text() {
        let input = json!({
            "code": "await sky.click({\"x\":840,\"y\":516}); await sky.type_text({\"text\":\"flight recorder secret\"});"
        });
        let actions = parse_sky_actions(&input);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].action, "click");
        assert_eq!(actions[1].public_payload["redacted"], true);
        assert_eq!(
            actions[1].sensitive_text.as_deref(),
            Some("flight recorder secret")
        );
        assert!(
            !actions[1]
                .public_payload
                .to_string()
                .contains("flight recorder secret")
        );
    }

    #[test]
    fn ignores_non_node_inputs() {
        assert!(parse_sky_actions(&json!({ "path": "x" })).is_empty());
    }

    #[test]
    fn extracts_type_text_from_real_computer_use_call_shape() {
        let input = json!({
            "code": r#"{
                const observation = globalThis.state;
                await sky.type_text({
                    window: observation.window,
                    text: "CdxVidExt live keyboard encryption verification"
                });
            }"#
        });
        let actions = parse_sky_actions(&input);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].public_payload["text_length"], 47);
        assert_eq!(
            actions[0].sensitive_text.as_deref(),
            Some("CdxVidExt live keyboard encryption verification")
        );
        assert!(
            !actions[0]
                .public_payload
                .to_string()
                .contains("keyboard encryption")
        );
    }
}
