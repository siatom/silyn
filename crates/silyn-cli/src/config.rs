//! Persistent CLI configuration.
//!
//! `silyn` reads `$SILYN_HOME/config.toml` (or the file named by `--config`)
//! before parsing the request, so the common run parameters do not have to be
//! repeated on the command line. Precedence is flags, then this file, then the
//! environment defaults.
//!
//! Secrets are never stored here. The provider still reads its key from the
//! environment variable named by `SILYN_LLM_API_KEY_ENV`.

use std::env;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, SessionId};

/// Contents of the configuration file. Every field is optional, so an empty
/// file is the same as no file.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub run: RunConfig,
    #[serde(default)]
    pub agent: AgentConfig,
}

/// Defaults for `silyn <prompt>` and `silyn run`.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    /// Model id recorded on the run.
    pub model: Option<String>,
    /// Use the deterministic offline provider.
    pub offline: Option<bool>,
    pub max_steps: Option<u32>,
    pub max_wall_secs: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    /// Capabilities granted to every run. Flags replace this list.
    pub capabilities: Option<Vec<String>>,
}

/// Identity reused by the prompt shortcut.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Agent that owns new sessions. Defaults to the newest agent, creating
    /// one owned by [`owner`](Self::owner) when none exists.
    pub id: Option<AgentId>,
    /// Existing session to reuse. When set, `id` and `owner` are ignored.
    pub session: Option<SessionId>,
    /// Owner label for the agent created on first use.
    pub owner: Option<String>,
}

/// Where the configuration came from and what it resolved to.
#[derive(Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub loaded: bool,
    pub config: ConfigFile,
}

impl LoadedConfig {
    /// Loads the file named by `explicit`, or `$SILYN_HOME/config.toml`.
    ///
    /// A missing implicit file is not an error; a missing `--config` target is,
    /// because the caller named it.
    pub fn load(explicit: Option<&Path>) -> Result<Self, SilynError> {
        let path = explicit.map_or_else(|| home_dir().join(CONFIG_FILE), Path::to_path_buf);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
                return Ok(Self {
                    path,
                    loaded: false,
                    config: ConfigFile::default(),
                });
            }
            Err(error) => {
                return Err(SilynError::Storage(format!(
                    "cannot read config file {}: {error}",
                    path.display()
                )));
            }
        };

        let config: ConfigFile = toml::from_str(&text).map_err(|error| {
            SilynError::Schema(format!("invalid config file {}: {error}", path.display()))
        })?;
        Ok(Self {
            path,
            loaded: true,
            config,
        })
    }
}

/// File name inside the Silyn home directory.
pub const CONFIG_FILE: &str = "config.toml";

pub fn home_dir() -> PathBuf {
    env::var("SILYN_HOME").map_or_else(|_| PathBuf::from(".silyn"), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_keeps_defaults() {
        let config: ConfigFile = toml::from_str("").expect("empty config");
        assert!(config.run.model.is_none());
        assert!(config.agent.session.is_none());
    }

    #[test]
    fn ids_parse_from_strings() {
        let config: ConfigFile = toml::from_str(
            r#"
[agent]
session = "0192f0a0-0000-7000-8000-000000000000"
"#,
        )
        .expect("config");
        assert!(config.agent.session.is_some());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = toml::from_str::<ConfigFile>("[run]\nmax_stpes = 3\n").expect_err("typo");
        assert!(error.to_string().contains("max_stpes"));
    }
}
