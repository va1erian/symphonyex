//! Shared `---` YAML front matter + Markdown body parsing, used by `WORKFLOW.md`
//! and by the local filesystem tracker adapter's issue files.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FrontMatterError {
    #[error("front matter did not parse as YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("front matter did not decode to a map/object")]
    NotAMap,
}

/// Returns `(front_matter_map, trimmed_body)`. If the input has no `---` front matter
/// block, returns an empty map and the whole input (trimmed) as body.
pub fn split(raw: &str) -> Result<(serde_yaml::Value, String), FrontMatterError> {
    let Some(after_first) = raw.strip_prefix("---") else {
        return Ok((serde_yaml::Value::Mapping(Default::default()), raw.trim().to_string()));
    };
    let after_first = after_first.strip_prefix('\n').unwrap_or(after_first);
    let Some(end) = find_end(after_first) else {
        return Ok((serde_yaml::Value::Mapping(Default::default()), raw.trim().to_string()));
    };
    let (front_matter, rest) = after_first.split_at(end);
    let rest = strip_closing_marker(rest);

    let value: serde_yaml::Value = if front_matter.trim().is_empty() {
        serde_yaml::Value::Mapping(Default::default())
    } else {
        serde_yaml::from_str(front_matter)?
    };

    let value = match value {
        serde_yaml::Value::Null => serde_yaml::Value::Mapping(Default::default()),
        serde_yaml::Value::Mapping(_) => value,
        _ => return Err(FrontMatterError::NotAMap),
    };

    Ok((value, rest.trim().to_string()))
}

fn find_end(after_first: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in after_first.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn strip_closing_marker(rest: &str) -> &str {
    let rest = rest.strip_prefix("---").unwrap_or(rest);
    rest.strip_prefix('\r').unwrap_or(rest).strip_prefix('\n').unwrap_or(rest)
}
