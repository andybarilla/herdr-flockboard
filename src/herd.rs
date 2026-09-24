//! Live agent data from the herdr CLI, behind the `HerdControl` trait so the
//! dashboard's state derivation is testable without a herdr server. Modeled
//! on herdr-scuttlebutt's `herd.rs`.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentInfo {
    pub agent: String,
    /// Raw `agent_status` string from herdr (`idle`, `working`, ...); mapped
    /// to `state::AgentStatus` at the derivation layer so new herdr statuses
    /// surface as `Unknown` instead of breaking parsing.
    pub status: String,
    pub cwd: String,
    pub focused: bool,
    pub workspace_id: String,
    pub pane_id: String,
    pub terminal_title: String,
}

pub trait HerdControl {
    fn list_agents(&self) -> Result<Vec<AgentInfo>>;
}

/// `herdr agent list` prints one JSON envelope:
/// `{"id": "...", "result": {"agents": [...], "type": "agent_list"}}`, or an
/// `error` member when the CLI itself failed (e.g. no server running).
#[derive(Deserialize)]
struct Envelope {
    result: Option<AgentsResult>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct AgentsResult {
    #[serde(default)]
    agents: Vec<RawAgent>,
}

/// Fields the board reads. Everything else herdr emits is ignored; anything
/// momentarily absent defaults rather than failing the whole listing.
#[derive(Deserialize)]
struct RawAgent {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    agent_status: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    terminal_title: String,
    #[serde(default)]
    terminal_title_stripped: String,
}

pub fn parse_agent_list(json: &str) -> Result<Vec<AgentInfo>> {
    let envelope: Envelope =
        serde_json::from_str(json).context("parsing `herdr agent list` output")?;
    if let Some(error) = envelope.error {
        bail!("herdr agent list: {error}");
    }
    let agents = envelope.result.map(|r| r.agents).ok_or_else(|| {
        anyhow::anyhow!("herdr agent list: envelope had neither result nor error")
    })?;
    Ok(agents
        .into_iter()
        .map(|raw| AgentInfo {
            agent: raw.agent,
            status: raw.agent_status,
            cwd: raw.cwd,
            focused: raw.focused,
            workspace_id: raw.workspace_id,
            pane_id: raw.pane_id,
            // The stripped title drops herdr's decoration; fall back to the
            // raw title when the stripped one is absent.
            terminal_title: if raw.terminal_title_stripped.is_empty() {
                raw.terminal_title
            } else {
                raw.terminal_title_stripped
            },
        })
        .collect())
}

/// Real implementation: shells out to the herdr CLI. `HERDR_BIN_PATH`
/// overrides the binary name, matching the plugin scripts' convention.
pub struct HerdCli {
    bin: String,
}

impl Default for HerdCli {
    fn default() -> Self {
        Self {
            bin: std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string()),
        }
    }
}

impl HerdControl for HerdCli {
    fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        let out = std::process::Command::new(&self.bin)
            .args(["agent", "list"])
            .output()
            .with_context(|| format!("spawning `{} agent list`", self.bin))?;
        if !out.status.success() {
            bail!(
                "`{} agent list` failed: {}",
                self.bin,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        parse_agent_list(std::str::from_utf8(&out.stdout).context("herdr output was not UTF-8")?)
    }
}

/// Test double: replays a canned listing or error, no herdr server needed.
#[cfg(test)]
pub struct FakeHerd {
    pub outcome: std::result::Result<Vec<AgentInfo>, String>,
}

#[cfg(test)]
impl HerdControl for FakeHerd {
    fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        match &self.outcome {
            Ok(agents) => Ok(agents.clone()),
            Err(e) => Err(anyhow::anyhow!(e.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed capture of real `herdr agent list` output (one working agent),
    /// so the parser is pinned to the observed wire shape.
    const SAMPLE: &str = r#"{"id":"cli:agent:list","result":{"agents":[{"agent":"pi","agent_session":{"agent":"pi","kind":"path","source":"herdr:pi","value":"/home/andy/.pi/agent/sessions/x.jsonl"},"agent_status":"working","cwd":"/home/andy/dev/andybarilla/herdr-flockboard","focused":true,"foreground_cwd":"/home/andy/dev/andybarilla/herdr-flockboard","pane_id":"wEG:p1","revision":2,"screen_detection_skipped":true,"state_change_seq":1302,"tab_id":"wEG:t1","terminal_id":"term_65c3dbe2718a151","terminal_title":"π - herdr-flockboard","terminal_title_stripped":"π - herdr-flockboard","workspace_id":"wEG"}],"type":"agent_list"}}"#;

    #[test]
    fn parses_observed_envelope() {
        let agents = parse_agent_list(SAMPLE).unwrap();
        assert_eq!(
            agents,
            vec![AgentInfo {
                agent: "pi".to_string(),
                status: "working".to_string(),
                cwd: "/home/andy/dev/andybarilla/herdr-flockboard".to_string(),
                focused: true,
                workspace_id: "wEG".to_string(),
                pane_id: "wEG:p1".to_string(),
                terminal_title: "π - herdr-flockboard".to_string(),
            }]
        );
    }

    #[test]
    fn empty_session_parses_as_no_agents() {
        let agents = parse_agent_list(
            r#"{"id":"cli:agent:list","result":{"agents":[],"type":"agent_list"}}"#,
        )
        .unwrap();
        assert!(agents.is_empty());
    }

    #[test]
    fn error_envelope_is_an_error_not_an_empty_board() {
        let err =
            parse_agent_list(r#"{"id":"cli:agent:list","error":{"message":"server not running"}}"#)
                .unwrap_err();
        assert!(err.to_string().contains("server not running"));
    }

    #[test]
    fn missing_optional_fields_default() {
        let agents = parse_agent_list(
            r#"{"result":{"agents":[{"agent":"pi","agent_status":"idle","cwd":"/tmp/x"}]}}"#,
        )
        .unwrap();
        assert_eq!(agents[0].workspace_id, "");
        assert!(!agents[0].focused);
        assert_eq!(agents[0].terminal_title, "");
    }

    #[test]
    fn fake_replays_canned_outcome() {
        let fake = FakeHerd {
            outcome: Ok(vec![AgentInfo {
                agent: "pi".to_string(),
                status: "idle".to_string(),
                cwd: "/tmp/x".to_string(),
                focused: false,
                workspace_id: "w1".to_string(),
                pane_id: "w1:p1".to_string(),
                terminal_title: "t".to_string(),
            }]),
        };
        assert_eq!(fake.list_agents().unwrap().len(), 1);
        let fake = FakeHerd {
            outcome: Err("no server".to_string()),
        };
        assert!(fake.list_agents().is_err());
    }
}
