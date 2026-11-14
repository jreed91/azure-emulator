//! The relaxed JSON Resource Manager accepts in request bodies: comments
//! (`//` and `/* */`), unquoted property names and trailing commas.
//!
//! The Azure CLI relies on this: `az deployment ... --template-file` sends
//! `{"properties": {..., template:{...}}}`, with the template's own text
//! (comments and all) after an unquoted `template` key.

use serde_json::Value;

/// Parses `bytes` as relaxed JSON, or `Err` if even that fails.
pub fn parse(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    let text = String::from_utf8_lossy(bytes);
    serde_json::from_str(&to_strict(&text))
}

/// Rewrites relaxed JSON as strict JSON. Strings are copied untouched.
fn to_strict(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                // A string, with its escapes, as it is.
                out.push(c);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if chars[i - 1] == '"' {
                        break;
                    }
                }
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                // Dropped when only whitespace or comments come before the closing bracket.
                let mut j = i + 1;
                while j < chars.len() {
                    if chars[j].is_whitespace() {
                        j += 1;
                    } else if chars[j] == '/' && chars.get(j + 1) == Some(&'/') {
                        while j < chars.len() && chars[j] != '\n' {
                            j += 1;
                        }
                    } else if chars[j] == '/' && chars.get(j + 1) == Some(&'*') {
                        j += 2;
                        while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '/') {
                            j += 1;
                        }
                        j += 2;
                    } else {
                        break;
                    }
                }
                if !matches!(chars.get(j), Some('}' | ']')) {
                    out.push(',');
                }
                i += 1;
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let mut j = i;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                // A word followed by a colon is a property name; true, false and
                // null are left as they are.
                if chars.get(j) == Some(&':') {
                    out.push('"');
                    out.push_str(&word);
                    out.push('"');
                } else {
                    out.push_str(&word);
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_azure_cli_deployment_body() {
        let body = "{\"properties\": {\"parameters\": {}, \"mode\": \"Incremental\", template:{\r\n  // a comment\r\n  \"$schema\": \"s\", /* another */ \"resources\": [1, 2,],\r\n}}}";
        assert_eq!(
            parse(body.as_bytes()).unwrap(),
            json!({"properties": {"parameters": {}, "mode": "Incremental", "template": {"$schema": "s", "resources": [1, 2]}}})
        );
    }

    #[test]
    fn strings_are_left_alone() {
        let body = r#"{"a": "x: // not a comment, /* nor this */ ,]", b: true, "c": null}"#;
        assert_eq!(parse(body.as_bytes()).unwrap(), json!({"a": "x: // not a comment, /* nor this */ ,]", "b": true, "c": null}));
    }

    #[test]
    fn garbage_is_still_refused() {
        assert!(parse(b"{not json").is_err());
    }
}
