//! Strict prompt template rendering (Section 5.4, 12).
//!
//! A minimal, Liquid-inspired subset: `{{ path.to.field }}`, `{{ path | filter }}`, and
//! `{% for x in path %}...{% endfor %}`. Unknown variables (a path segment that does not
//! exist in the current context) and unknown filters both fail rendering, per spec.

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum TemplateError {
    #[error("template_parse_error: {0}")]
    Parse(String),
    #[error("template_render_error: {0}")]
    Render(String),
}

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Output {
        path: Vec<String>,
        filters: Vec<Filter>,
    },
    For {
        var: String,
        path: Vec<String>,
        body: Vec<Node>,
    },
}

#[derive(Debug, Clone)]
struct Filter {
    name: String,
    arg: Option<String>,
}

enum Token {
    Text(String),
    Output(String),
    Tag(String),
}

/// Render `template` against a JSON-serializable context (Section 12.2: convert issue
/// object keys to strings, preserve nested arrays/maps for iteration).
pub fn render(template: &str, context: &Value) -> Result<String, TemplateError> {
    let tokens = tokenize(template)?;
    let mut pos = 0usize;
    let nodes = parse_nodes(&tokens, &mut pos, &[])?;
    if pos != tokens.len() {
        return Err(TemplateError::Parse("unexpected closing tag".to_string()));
    }
    let mut scopes: Vec<(String, Value)> = Vec::new();
    render_nodes(&nodes, context, &mut scopes)
}

fn tokenize(template: &str) -> Result<Vec<Token>, TemplateError> {
    let mut tokens = Vec::new();
    let mut rest = template;
    loop {
        let next_output = rest.find("{{");
        let next_tag = rest.find("{%");
        let next = match (next_output, next_tag) {
            (Some(o), Some(t)) => Some(o.min(t)),
            (Some(o), None) => Some(o),
            (None, Some(t)) => Some(t),
            (None, None) => None,
        };
        let Some(idx) = next else {
            if !rest.is_empty() {
                tokens.push(Token::Text(rest.to_string()));
            }
            break;
        };
        if idx > 0 {
            tokens.push(Token::Text(rest[..idx].to_string()));
        }
        let is_output = rest[idx..].starts_with("{{");
        let (open, close) = if is_output { ("{{", "}}") } else { ("{%", "%}") };
        let after_open = &rest[idx + open.len()..];
        let Some(end) = after_open.find(close) else {
            return Err(TemplateError::Parse(format!("unterminated '{open}'")));
        };
        let inner = after_open[..end].trim().to_string();
        tokens.push(if is_output {
            Token::Output(inner)
        } else {
            Token::Tag(inner)
        });
        rest = &after_open[end + close.len()..];
    }
    Ok(tokens)
}

fn parse_nodes(
    tokens: &[Token],
    pos: &mut usize,
    stop_words: &[&str],
) -> Result<Vec<Node>, TemplateError> {
    let mut nodes = Vec::new();
    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Text(s) => {
                nodes.push(Node::Text(s.clone()));
                *pos += 1;
            }
            Token::Output(inner) => {
                nodes.push(parse_output(inner)?);
                *pos += 1;
            }
            Token::Tag(inner) => {
                let word = inner.split_whitespace().next().unwrap_or("");
                if stop_words.contains(&word) {
                    return Ok(nodes);
                }
                match word {
                    "for" => {
                        *pos += 1;
                        let (var, path) = parse_for_expr(inner)?;
                        let body = parse_nodes(tokens, pos, &["endfor"])?;
                        expect_tag(tokens, pos, "endfor")?;
                        nodes.push(Node::For { var, path, body });
                    }
                    "" => return Err(TemplateError::Parse("empty tag".to_string())),
                    other => {
                        return Err(TemplateError::Parse(format!("unknown tag '{other}'")));
                    }
                }
            }
        }
    }
    Ok(nodes)
}

fn expect_tag(tokens: &[Token], pos: &mut usize, word: &str) -> Result<(), TemplateError> {
    match tokens.get(*pos) {
        Some(Token::Tag(inner)) if inner.split_whitespace().next() == Some(word) => {
            *pos += 1;
            Ok(())
        }
        _ => Err(TemplateError::Parse(format!("expected '{{% {word} %}}'"))),
    }
}

fn parse_path(s: &str) -> Result<Vec<String>, TemplateError> {
    if s.is_empty() {
        return Err(TemplateError::Parse("empty variable path".to_string()));
    }
    Ok(s.split('.').map(|p| p.trim().to_string()).collect())
}

fn parse_output(inner: &str) -> Result<Node, TemplateError> {
    let mut parts = inner.split('|');
    let path_part = parts.next().unwrap_or_default().trim();
    let path = parse_path(path_part)?;
    let mut filters = Vec::new();
    for filter_part in parts {
        let filter_part = filter_part.trim();
        let (name, arg) = match filter_part.split_once(':') {
            Some((n, a)) => (n.trim().to_string(), Some(unquote(a.trim()))),
            None => (filter_part.to_string(), None),
        };
        if name.is_empty() {
            return Err(TemplateError::Parse("empty filter name".to_string()));
        }
        filters.push(Filter { name, arg });
    }
    Ok(Node::Output { path, filters })
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// `for <var> in <path>`
fn parse_for_expr(inner: &str) -> Result<(String, Vec<String>), TemplateError> {
    let rest = inner
        .strip_prefix("for ")
        .ok_or_else(|| TemplateError::Parse(format!("malformed for tag: '{inner}'")))?;
    let (var, path_part) = rest
        .split_once(" in ")
        .ok_or_else(|| TemplateError::Parse(format!("malformed for tag: '{inner}'")))?;
    let var = var.trim().to_string();
    if var.is_empty() {
        return Err(TemplateError::Parse("for loop missing variable name".to_string()));
    }
    Ok((var, parse_path(path_part.trim())?))
}

const KNOWN_FILTERS: &[&str] = &["upcase", "downcase", "default"];

fn apply_filter(value: Value, filter: &Filter) -> Result<Value, TemplateError> {
    if !KNOWN_FILTERS.contains(&filter.name.as_str()) {
        return Err(TemplateError::Render(format!(
            "unknown filter '{}'",
            filter.name
        )));
    }
    match filter.name.as_str() {
        "upcase" => Ok(Value::String(display(&value).to_uppercase())),
        "downcase" => Ok(Value::String(display(&value).to_lowercase())),
        "default" => {
            let is_empty = matches!(&value, Value::Null)
                || matches!(&value, Value::String(s) if s.is_empty());
            if is_empty {
                Ok(Value::String(filter.arg.clone().unwrap_or_default()))
            } else {
                Ok(value)
            }
        }
        _ => unreachable!(),
    }
}

fn display(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Resolve a dotted path against loop scopes (innermost first) then the root context.
/// Every path segment MUST exist as a key; a missing key is an unknown-variable error
/// even if an earlier segment resolved to `null`... except the final segment, whose
/// value MAY legitimately be `null`.
fn resolve(path: &[String], scopes: &[(String, Value)], root: &Value) -> Result<Value, TemplateError> {
    let (head, tail) = path.split_first().expect("path is never empty");

    let mut current = if let Some((_, v)) = scopes.iter().rev().find(|(name, _)| name == head) {
        v.clone()
    } else if let Some(v) = root.get(head) {
        v.clone()
    } else {
        return Err(TemplateError::Render(format!("unknown variable '{head}'")));
    };

    let mut consumed = head.clone();
    for segment in tail {
        match &current {
            Value::Object(map) => {
                if let Some(v) = map.get(segment) {
                    current = v.clone();
                } else {
                    return Err(TemplateError::Render(format!(
                        "unknown variable '{consumed}.{segment}'"
                    )));
                }
            }
            Value::Null => {
                return Err(TemplateError::Render(format!(
                    "cannot access '{segment}' on null value '{consumed}'"
                )));
            }
            _ => {
                return Err(TemplateError::Render(format!(
                    "cannot access '{segment}' on non-object value '{consumed}'"
                )));
            }
        }
        consumed = format!("{consumed}.{segment}");
    }
    Ok(current)
}

fn render_nodes(
    nodes: &[Node],
    root: &Value,
    scopes: &mut Vec<(String, Value)>,
) -> Result<String, TemplateError> {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(s) => out.push_str(s),
            Node::Output { path, filters } => {
                let mut value = resolve(path, scopes, root)?;
                for filter in filters {
                    value = apply_filter(value, filter)?;
                }
                out.push_str(&display(&value));
            }
            Node::For { var, path, body } => {
                let items = resolve(path, scopes, root)?;
                let items: Vec<Value> = match items {
                    Value::Null => Vec::new(),
                    Value::Array(items) => items,
                    _ => {
                        return Err(TemplateError::Render(format!(
                            "'{}' is not iterable",
                            path.join(".")
                        )));
                    }
                };
                for item in items {
                    scopes.push((var.clone(), item));
                    out.push_str(&render_nodes(body, root, scopes)?);
                    scopes.pop();
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_known_variables() {
        let ctx = json!({"issue": {"identifier": "ABC-1", "title": "Fix bug"}, "attempt": null});
        let out = render("{{ issue.identifier }}: {{ issue.title }}", &ctx).unwrap();
        assert_eq!(out, "ABC-1: Fix bug");
    }

    #[test]
    fn unknown_variable_fails() {
        let ctx = json!({"issue": {}, "attempt": null});
        let err = render("{{ issue.nope }}", &ctx).unwrap_err();
        assert!(matches!(err, TemplateError::Render(_)));
    }

    #[test]
    fn unknown_root_fails() {
        let ctx = json!({"issue": {}, "attempt": null});
        let err = render("{{ bogus }}", &ctx).unwrap_err();
        assert!(matches!(err, TemplateError::Render(_)));
    }

    #[test]
    fn null_leaf_renders_empty() {
        let ctx = json!({"issue": {"description": null}, "attempt": null});
        let out = render("[{{ issue.description }}]", &ctx).unwrap();
        assert_eq!(out, "[]");
    }

    #[test]
    fn unknown_filter_fails() {
        let ctx = json!({"issue": {"title": "x"}});
        let err = render("{{ issue.title | shout }}", &ctx).unwrap_err();
        assert!(matches!(err, TemplateError::Render(_)));
    }

    #[test]
    fn for_loop_iterates_labels() {
        let ctx = json!({"issue": {"labels": ["a", "b"]}});
        let out = render("{% for l in issue.labels %}[{{ l }}]{% endfor %}", &ctx).unwrap();
        assert_eq!(out, "[a][b]");
    }

    #[test]
    fn for_loop_over_objects() {
        let ctx = json!({"issue": {"blocked_by": [{"identifier": "X-1"}]}});
        let out = render(
            "{% for b in issue.blocked_by %}{{ b.identifier }}{% endfor %}",
            &ctx,
        )
        .unwrap();
        assert_eq!(out, "X-1");
    }

    #[test]
    fn default_filter_substitutes_null() {
        let ctx = json!({"issue": {"description": null}});
        let out = render("{{ issue.description | default: \"none\" }}", &ctx).unwrap();
        assert_eq!(out, "none");
    }
}
