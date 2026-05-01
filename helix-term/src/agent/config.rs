use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct AgentConfig {
    pub enable: bool,
    pub default_agent: String,
    pub auto_context_on_open: bool,
    pub include_theme: bool,
    pub include_command_history: bool,
    pub include_visible_buffer: bool,
    pub include_diagnostics: bool,
    pub require_approval_for_shell: bool,
    pub servers: BTreeMap<String, AgentServerConfig>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct AgentServerConfig {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunchConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

impl AgentConfig {
    pub fn launch_config(&self) -> anyhow::Result<AgentLaunchConfig> {
        let server = self.servers.get(&self.default_agent).ok_or_else(|| {
            anyhow::anyhow!("agent server '{}' is not configured", self.default_agent)
        })?;

        if server.command.trim().is_empty() {
            anyhow::bail!("agent server '{}' has an empty command", self.default_agent);
        }

        Ok(AgentLaunchConfig {
            name: self.default_agent.clone(),
            command: server.command.clone(),
            args: server.args.clone(),
        })
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enable: false,
            default_agent: "codex".to_string(),
            auto_context_on_open: true,
            include_theme: true,
            include_command_history: true,
            include_visible_buffer: true,
            include_diagnostics: true,
            require_approval_for_shell: true,
            servers: default_servers(),
        }
    }
}

fn default_servers() -> BTreeMap<String, AgentServerConfig> {
    BTreeMap::from([(
        "codex".to_string(),
        AgentServerConfig {
            command: "codex".to_string(),
            args: vec!["acp".to_string()],
        },
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_config() {
        let config: AgentConfig = toml::from_str(
            r#"
            enable = true
            default-agent = "local"

            [servers.local]
            command = "agent"
            args = ["--acp"]
            "#,
        )
        .unwrap();

        assert!(config.enable);
        assert_eq!(config.default_agent, "local");
        assert_eq!(config.servers["local"].command, "agent");
        assert_eq!(config.servers["local"].args, ["--acp"]);
    }

    #[test]
    fn resolves_launch_config() {
        let config: AgentConfig = toml::from_str(
            r#"
            default-agent = "local"

            [servers.local]
            command = "agent"
            args = ["--acp"]
            "#,
        )
        .unwrap();

        let launch = config.launch_config().unwrap();
        assert_eq!(launch.name, "local");
        assert_eq!(launch.command, "agent");
        assert_eq!(launch.args, ["--acp"]);
    }

    #[test]
    fn rejects_missing_default_agent() {
        let config: AgentConfig = toml::from_str(
            r#"
            default-agent = "missing"

            [servers.local]
            command = "agent"
            "#,
        )
        .unwrap();

        assert!(config.launch_config().is_err());
    }
}
