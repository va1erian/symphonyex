//! `WORKFLOW.md` discovery and parsing (Section 5).

use crate::domain::WorkflowDefinition;
use crate::frontmatter::{self, FrontMatterError};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("missing_workflow_file: {0}")]
    MissingFile(PathBuf),
    #[error("workflow_parse_error: {0}")]
    ParseError(#[from] serde_yaml::Error),
    #[error("workflow_front_matter_not_a_map: front matter did not decode to a map/object")]
    FrontMatterNotAMap,
}

impl From<FrontMatterError> for WorkflowError {
    fn from(e: FrontMatterError) -> Self {
        match e {
            FrontMatterError::Yaml(err) => WorkflowError::ParseError(err),
            FrontMatterError::NotAMap => WorkflowError::FrontMatterNotAMap,
        }
    }
}

/// Resolve the workflow file path per Section 5.1 precedence:
/// 1. explicit runtime path, 2. `WORKFLOW.md` in cwd.
pub fn resolve_workflow_path(explicit: Option<&Path>) -> PathBuf {
    match explicit {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from("WORKFLOW.md"),
    }
}

/// Load and parse a `WORKFLOW.md` file into front matter config + trimmed prompt body.
pub fn load(path: &Path) -> Result<WorkflowDefinition, WorkflowError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|_| WorkflowError::MissingFile(path.to_path_buf()))?;
    parse(&raw)
}

/// Parsing rules (Section 5.2): see `frontmatter::split`. Front matter absence yields an
/// empty config map; front matter MUST decode to a map/object; body is trimmed.
pub fn parse(raw: &str) -> Result<WorkflowDefinition, WorkflowError> {
    let (config, prompt_template) = frontmatter::split(raw)?;
    Ok(WorkflowDefinition {
        config,
        prompt_template,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_front_matter_and_body() {
        let raw = "---\ntracker:\n  kind: local\n---\nHello {{ issue.title }}\n";
        let wf = parse(raw).unwrap();
        assert_eq!(wf.prompt_template, "Hello {{ issue.title }}");
        let tracker = wf.config.get("tracker").unwrap();
        assert_eq!(tracker.get("kind").unwrap().as_str().unwrap(), "local");
    }

    #[test]
    fn no_front_matter_is_empty_config() {
        let raw = "Just a prompt body.";
        let wf = parse(raw).unwrap();
        assert_eq!(wf.prompt_template, "Just a prompt body.");
        assert!(wf.config.as_mapping().unwrap().is_empty());
    }

    #[test]
    fn non_map_front_matter_errors() {
        let raw = "---\n- a\n- b\n---\nbody\n";
        assert!(matches!(parse(raw), Err(WorkflowError::FrontMatterNotAMap)));
    }

    #[test]
    fn trims_prompt_body() {
        let raw = "---\n{}\n---\n\n\n  Hello  \n\n";
        let wf = parse(raw).unwrap();
        assert_eq!(wf.prompt_template, "Hello");
    }

    #[test]
    fn missing_file_errors() {
        let err = load(Path::new("does/not/exist/WORKFLOW.md")).unwrap_err();
        assert!(matches!(err, WorkflowError::MissingFile(_)));
    }
}
