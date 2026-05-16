//! Prompt-template rendering.
//!
//! Agent definitions carry a verbatim `prompt_template: String`. We
//! render it with a *minimal* `{{key}}` substitution against the input
//! JSON (and resolved knowledge, injected by the executor under the
//! reserved `knowledge` key). This is intentionally **not** a template
//! engine: no conditionals, no loops, no arbitrary code execution. A
//! template that references an undefined key is rejected — silent
//! empty-substitution would let a malformed agent produce confidently
//! wrong output, which is exactly the failure mode the conservative
//! culture forbids.
//!
//! ## Substitution rules
//!
//! - `{{foo}}` → the JSON value at top-level key `foo` of the context.
//! - `{{foo.bar}}` → dotted path into nested objects.
//! - String values substitute verbatim (no quotes). Non-string values
//!   substitute as their compact JSON encoding.
//! - `{{{{` is a literal `{{`; `}}}}` is a literal `}}` (escaping).
//! - An unresolved key is a hard [`RenderError::UndefinedKey`].

use serde_json::Value;
use thiserror::Error;

/// Errors raised while rendering a prompt template.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum RenderError {
    /// A `{{...}}` placeholder referenced a key not present in the
    /// rendering context.
    #[error("template references undefined key `{0}`")]
    UndefinedKey(String),
    /// A `{{` was opened but never closed before end-of-template.
    #[error("unterminated `{{{{` placeholder in template")]
    UnterminatedPlaceholder,
    /// A placeholder body was empty (`{{}}`).
    #[error("empty `{{{{}}}}` placeholder in template")]
    EmptyPlaceholder,
}

/// Render `template` against the JSON `context`.
///
/// See module docs for the substitution rules. The executor places the
/// agent input at the top level and injects resolved knowledge under
/// the reserved key `knowledge`.
pub fn render_prompt(template: &str, context: &Value) -> Result<String, RenderError> {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        // Escaped literal braces: `{{{{` -> `{{`, `}}}}` -> `}}`.
        if bytes[i] == b'{' && i + 3 < bytes.len() && &bytes[i..i + 4] == b"{{{{" {
            out.push_str("{{");
            i += 4;
            continue;
        }
        if bytes[i] == b'}' && i + 3 < bytes.len() && &bytes[i..i + 4] == b"}}}}" {
            out.push_str("}}");
            i += 4;
            continue;
        }
        if bytes[i] == b'{' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Find the closing `}}`.
            let rest = &template[i + 2..];
            let close = rest
                .find("}}")
                .ok_or(RenderError::UnterminatedPlaceholder)?;
            let key = rest[..close].trim();
            if key.is_empty() {
                return Err(RenderError::EmptyPlaceholder);
            }
            let value = lookup(context, key)
                .ok_or_else(|| RenderError::UndefinedKey(key.to_string()))?;
            out.push_str(&stringify(value));
            i += 2 + close + 2;
            continue;
        }
        // Ordinary byte — copy the whole UTF-8 char.
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&template[i..i + ch_len]);
        i += ch_len;
    }
    Ok(out)
}

/// Resolve a dotted path (`a.b.c`) into a JSON object/array tree.
///
/// A segment that parses as `usize` indexes into a JSON array (so
/// `knowledge.0.text` reaches the first resolved corpus); otherwise it
/// is an object key.
fn lookup<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = ctx;
    for seg in path.split('.') {
        cur = match (cur, seg.parse::<usize>()) {
            (Value::Array(arr), Ok(idx)) => arr.get(idx)?,
            _ => cur.get(seg)?,
        };
    }
    Some(cur)
}

/// Render a JSON value into prompt text: strings verbatim, everything
/// else as compact JSON.
fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Byte length of the UTF-8 sequence whose lead byte is `b`.
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitutes_top_level_string() {
        let ctx = json!({"name": "Cyrus"});
        assert_eq!(
            render_prompt("Hello {{name}}!", &ctx).unwrap(),
            "Hello Cyrus!"
        );
    }

    #[test]
    fn substitutes_dotted_path_and_non_string() {
        let ctx = json!({"opts": {"n": 5}});
        assert_eq!(
            render_prompt("n={{opts.n}}", &ctx).unwrap(),
            "n=5"
        );
    }

    #[test]
    fn undefined_key_is_error() {
        let ctx = json!({"a": 1});
        assert_eq!(
            render_prompt("{{missing}}", &ctx).unwrap_err(),
            RenderError::UndefinedKey("missing".to_string())
        );
    }

    #[test]
    fn escaped_braces_are_literal() {
        let ctx = json!({});
        assert_eq!(
            render_prompt("{{{{not a var}}}}", &ctx).unwrap(),
            "{{not a var}}"
        );
    }

    #[test]
    fn unicode_template_is_safe() {
        let ctx = json!({"q": "سلام"});
        assert_eq!(
            render_prompt("پرسش: {{q}}", &ctx).unwrap(),
            "پرسش: سلام"
        );
    }
}
