//! Post-deploy trigger detection: two poll-based mechanisms, no webhook receiver --
//! the code host's own deployment/pipeline status API (`RepoHost::
//! latest_successful_deploy`, `src/repo_host/{github,gitlab}.rs`) or a configured
//! command (`observability.validation.deploy_command`). Either can report a new
//! deploy; `production_validation::run` (the caller) doesn't care which one found it.
//!
//! Associating a detected deploy with the ticket that produced it is best-effort: the
//! host-status-API signal has no way to derive that on its own (that would need a
//! durable commit->issue mapping this codebase doesn't have yet -- see AIR-3's
//! artifact store), so it always reports `issue_id: None`. A configured command is
//! run by the *operator*, so it can simply echo the ticket id back in its output.

use crate::repo_host::RepoHost;
use async_trait::async_trait;

/// One detected deploy. `sha`/`identifier` dedupe repeated detections of the same
/// deploy (see `production_validation::run`); `issue_id` is populated only when the
/// signal itself can name the ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployEvent {
    pub sha: String,
    pub identifier: String,
    pub issue_id: Option<String>,
}

#[async_trait]
pub trait DeploySignal: Send + Sync {
    /// The latest deploy, if any. `Ok(None)` means "reachable, nothing deployed yet"
    /// -- never an error; a signal that can't be reached/run at all is `Err`.
    async fn latest_deploy(&self) -> Result<Option<DeployEvent>, String>;
}

pub struct HostDeploySignal<'a> {
    pub host: &'a dyn RepoHost,
}

#[async_trait]
impl<'a> DeploySignal for HostDeploySignal<'a> {
    async fn latest_deploy(&self) -> Result<Option<DeployEvent>, String> {
        Ok(self
            .host
            .latest_successful_deploy()
            .await?
            .map(|record| DeployEvent {
                sha: record.sha,
                identifier: record.identifier,
                issue_id: None,
            }))
    }
}

pub struct CommandDeploySignal {
    pub command: String,
}

#[async_trait]
impl DeploySignal for CommandDeploySignal {
    async fn latest_deploy(&self) -> Result<Option<DeployEvent>, String> {
        let stdout = run_command(&self.command).await?;
        parse_command_output(&stdout)
    }
}

async fn run_command(command: &str) -> Result<String, String> {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let output = cmd.output().await.map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "observability.validation.deploy_command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The configured command's stdout is either a bare commit sha, or a JSON object
/// `{"sha": "...", "issue_id": "..."}` (`issue_id` optional). Empty output means "no
/// deploy yet" -- the same "reachable, nothing to report" case `HostDeploySignal`
/// reports as `Ok(None)`, not an error.
fn parse_command_output(stdout: &str) -> Result<Option<DeployEvent>, String> {
    if stdout.is_empty() {
        return Ok(None);
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) {
        let sha = v
            .get("sha")
            .and_then(|s| s.as_str())
            .ok_or("deploy_command JSON output is missing a 'sha' field")?
            .to_string();
        let issue_id = v
            .get("issue_id")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        return Ok(Some(DeployEvent {
            identifier: sha.clone(),
            sha,
            issue_id,
        }));
    }
    Ok(Some(DeployEvent {
        identifier: stdout.to_string(),
        sha: stdout.to_string(),
        issue_id: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_output_means_no_deploy_yet() {
        assert_eq!(parse_command_output("").unwrap(), None);
    }

    #[test]
    fn bare_sha_output_is_accepted() {
        let event = parse_command_output("abc123").unwrap().unwrap();
        assert_eq!(event.sha, "abc123");
        assert_eq!(event.issue_id, None);
    }

    #[test]
    fn json_output_with_issue_id_is_parsed() {
        let event = parse_command_output(r#"{"sha": "abc123", "issue_id": "AIR-10"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(event.sha, "abc123");
        assert_eq!(event.issue_id.as_deref(), Some("AIR-10"));
    }

    #[test]
    fn json_output_without_issue_id_leaves_it_none() {
        let event = parse_command_output(r#"{"sha": "abc123"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(event.issue_id, None);
    }

    #[test]
    fn json_output_missing_sha_is_an_error() {
        assert!(parse_command_output(r#"{"issue_id": "AIR-10"}"#).is_err());
    }

    #[tokio::test]
    async fn command_deploy_signal_runs_the_configured_command() {
        let signal = CommandDeploySignal {
            command: "echo abc123".to_string(),
        };
        let event = signal.latest_deploy().await.unwrap().unwrap();
        assert_eq!(event.sha, "abc123");
    }

    #[tokio::test]
    async fn command_deploy_signal_surfaces_a_nonzero_exit_as_an_error() {
        let command = if cfg!(windows) { "exit /b 1" } else { "exit 1" };
        let signal = CommandDeploySignal {
            command: command.to_string(),
        };
        assert!(signal.latest_deploy().await.is_err());
    }
}
