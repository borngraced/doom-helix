use crate::keymap;
use crate::keymap::{merge_keys, KeyTrie};
use helix_loader::merge_toml_values;
use helix_view::editor::AgentConfig;
use helix_view::{document::Mode, theme};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Display;
use std::fs;
use std::io::Error as IOError;
use toml::de::Error as TomlError;
use toml::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub theme: Option<theme::Config>,
    pub keys: HashMap<Mode, KeyTrie>,
    pub editor: helix_view::editor::Config,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRaw {
    pub theme: Option<theme::Config>,
    pub keys: Option<HashMap<Mode, KeyTrie>>,
    pub editor: Option<toml::Value>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            theme: None,
            keys: keymap::default(),
            editor: helix_view::editor::Config::default(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigLoadError {
    BadConfig(TomlError),
    Error(IOError),
}

impl Default for ConfigLoadError {
    fn default() -> Self {
        ConfigLoadError::Error(IOError::new(std::io::ErrorKind::NotFound, "place holder"))
    }
}

impl Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigLoadError::BadConfig(err) => err.fmt(f),
            ConfigLoadError::Error(err) => err.fmt(f),
        }
    }
}

impl Config {
    pub fn load(
        global: Result<&String, ConfigLoadError>,
        local: Result<String, ConfigLoadError>,
    ) -> Result<Config, ConfigLoadError> {
        let global_config: Result<ConfigRaw, ConfigLoadError> =
            global.and_then(|file| toml::from_str(file).map_err(ConfigLoadError::BadConfig));
        let local_config: Result<ConfigRaw, ConfigLoadError> =
            local.and_then(|file| toml::from_str(&file).map_err(ConfigLoadError::BadConfig));
        let res = match (global_config, local_config) {
            (Ok(global), Ok(local)) => {
                let mut keys = keymap::default();
                if let Some(global_keys) = global.keys {
                    merge_keys(&mut keys, global_keys)
                }
                if let Some(local_keys) = local.keys {
                    merge_keys(&mut keys, local_keys)
                }

                let editor = match (global.editor, local.editor) {
                    (None, None) => helix_view::editor::Config::default(),
                    (None, Some(val)) | (Some(val), None) => {
                        val.try_into().map_err(ConfigLoadError::BadConfig)?
                    }
                    (Some(global), Some(local)) => merge_toml_values(global, local, 3)
                        .try_into()
                        .map_err(ConfigLoadError::BadConfig)?,
                };

                Config {
                    theme: local.theme.or(global.theme),
                    keys,
                    editor,
                }
            }
            // if any configs are invalid return that first
            (_, Err(ConfigLoadError::BadConfig(err)))
            | (Err(ConfigLoadError::BadConfig(err)), _) => {
                return Err(ConfigLoadError::BadConfig(err))
            }
            (Ok(config), Err(_)) | (Err(_), Ok(config)) => {
                let mut keys = keymap::default();
                if let Some(keymap) = config.keys {
                    merge_keys(&mut keys, keymap);
                }
                Config {
                    theme: config.theme,
                    keys,
                    editor: config.editor.map_or_else(
                        || Ok(helix_view::editor::Config::default()),
                        |val| val.try_into().map_err(ConfigLoadError::BadConfig),
                    )?,
                }
            }

            // these are just two io errors return the one for the global config
            (Err(err), Err(_)) => return Err(err),
        };

        Ok(res)
    }

    pub fn load_default() -> Result<Config, ConfigLoadError> {
        let global_config = read_optional_config(helix_loader::config_file())?;
        let local_config = fs::read_to_string(helix_loader::workspace_config_file())
            .map_err(ConfigLoadError::Error);

        let phony_config = ConfigLoadError::Error(IOError::other("hacky placeholder"));
        let global_parsed = match global_config.as_ref() {
            Some(global_config) => Config::load(Ok(global_config), Err(phony_config))?,
            None => Config::default(),
        };
        let trusted = matches!(
            helix_loader::workspace_trust::quick_query_workspace(global_parsed.editor.insecure),
            helix_loader::workspace_trust::TrustStatus::Trusted
        );

        let mut config = if trusted {
            match global_config.as_ref() {
                Some(global_config) => Config::load(Ok(global_config), local_config)?,
                None => Config::load(Err(ConfigLoadError::default()), local_config)?,
            }
        } else {
            global_parsed
        };

        let base_agent = agent_config_value(&config.editor.agent)?;
        let global_agent = read_optional_config(helix_loader::agent_config_file())?;
        let local_agent = if trusted {
            read_optional_config(helix_loader::workspace_agent_config_file())?
        } else {
            None
        };
        if let Some(agent) = load_agent_config(base_agent, global_agent.as_ref(), local_agent)? {
            config.editor.agent = agent;
        }

        Ok(config)
    }
}

fn read_optional_config(path: std::path::PathBuf) -> Result<Option<String>, ConfigLoadError> {
    match fs::read_to_string(path) {
        Ok(config) => Ok(Some(config)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ConfigLoadError::Error(err)),
    }
}

fn load_agent_config(
    base: Value,
    global: Option<&String>,
    local: Option<String>,
) -> Result<Option<AgentConfig>, ConfigLoadError> {
    let global = global
        .map(|config| parse_agent_config_value(config))
        .transpose()?;
    let local = local
        .as_ref()
        .map(|config| parse_agent_config_value(config))
        .transpose()?;

    let value = match (global, local) {
        (Some(global), Some(local)) => {
            merge_toml_values(merge_toml_values(base, global, 3), local, 3)
        }
        (Some(global), None) => merge_toml_values(base, global, 3),
        (None, Some(local)) => merge_toml_values(base, local, 3),
        (None, None) => {
            return Ok(None);
        }
    };

    value
        .try_into()
        .map(Some)
        .map_err(ConfigLoadError::BadConfig)
}

fn parse_agent_config_value(config: &str) -> Result<toml::Value, ConfigLoadError> {
    let value = Value::Table(
        config
            .parse::<toml::Table>()
            .map_err(ConfigLoadError::BadConfig)?,
    );

    Ok(value
        .get("editor")
        .and_then(|editor| editor.get("agent"))
        .or_else(|| value.get("agent"))
        .cloned()
        .unwrap_or(value))
}

fn agent_config_value(agent: &AgentConfig) -> Result<Value, ConfigLoadError> {
    let config = toml::to_string(agent)
        .map_err(|err| ConfigLoadError::Error(IOError::new(std::io::ErrorKind::InvalidData, err)))?
        .parse::<toml::Table>()
        .map_err(ConfigLoadError::BadConfig)?;

    Ok(Value::Table(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Config {
        fn load_test(config: &str) -> Config {
            Config::load(Ok(&config.to_owned()), Err(ConfigLoadError::default())).unwrap()
        }
    }

    #[test]
    fn parsing_keymaps_config_file() {
        use crate::keymap;
        use helix_core::hashmap;
        use helix_view::document::Mode;

        let sample_keymaps = r#"
            [keys.insert]
            y = "move_line_down"
            S-C-a = "delete_selection"

            [keys.normal]
            A-F12 = "move_next_word_end"
        "#;

        let mut keys = keymap::default();
        merge_keys(
            &mut keys,
            hashmap! {
                Mode::Insert => keymap!({ "Insert mode"
                    "y" => move_line_down,
                    "S-C-a" => delete_selection,
                }),
                Mode::Normal => keymap!({ "Normal mode"
                    "A-F12" => move_next_word_end,
                }),
            },
        );

        assert_eq!(
            Config::load_test(sample_keymaps),
            Config {
                keys,
                ..Default::default()
            }
        );
    }

    #[test]
    fn keys_resolve_to_correct_defaults() {
        // From serde default
        let default_keys = Config::load_test("").keys;
        assert_eq!(default_keys, keymap::default());

        // From the Default trait
        let default_keys = Config::default().keys;
        assert_eq!(default_keys, keymap::default());
    }

    #[test]
    fn agent_config_accepts_bare_agent_toml() {
        let base = agent_config_value(&AgentConfig::default()).unwrap();
        let agent = load_agent_config(
            base,
            Some(
                &r#"
                    enable = true
                    name = "claude"
                    command = "claude-code-acp"
                    panel-size = 42
                "#
                .to_string(),
            ),
            None,
        )
        .unwrap()
        .unwrap();

        assert!(agent.enable);
        assert_eq!(agent.name, "claude");
        assert_eq!(agent.command, "claude-code-acp");
        assert_eq!(agent.panel_size, 42);
    }

    #[test]
    fn agent_config_accepts_nested_agent_toml() {
        let base = agent_config_value(&AgentConfig::default()).unwrap();
        let agent = load_agent_config(
            base,
            Some(
                &r#"
                    [editor.agent]
                    enable = true
                    name = "codex"
                    command = "codex-acp"
                "#
                .to_string(),
            ),
            None,
        )
        .unwrap()
        .unwrap();

        assert!(agent.enable);
        assert_eq!(agent.name, "codex");
        assert_eq!(agent.command, "codex-acp");
    }

    #[test]
    fn agent_toml_merges_over_existing_agent_config() {
        let mut base_agent = AgentConfig::default();
        base_agent.enable = true;
        base_agent.name = "codex".to_string();
        base_agent.command = "codex-acp".to_string();
        base_agent.panel_size = 30;

        let base = agent_config_value(&base_agent).unwrap();
        let agent = load_agent_config(
            base,
            Some(&r#"panel-size = 45"#.to_string()),
            Some("include-diagnostics = false".to_string()),
        )
        .unwrap()
        .unwrap();

        assert!(agent.enable);
        assert_eq!(agent.name, "codex");
        assert_eq!(agent.command, "codex-acp");
        assert_eq!(agent.panel_size, 45);
        assert!(!agent.include_diagnostics);
    }
}
