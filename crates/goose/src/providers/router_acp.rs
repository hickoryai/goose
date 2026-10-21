use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::acp::{
    configured_model_for_provider, extension_configs_to_mcp_servers, AcpProvider,
    AcpProviderConfig, ACP_CURRENT_MODEL,
};
use crate::config::search_path::SearchPaths;
use crate::config::{Config, ConfigError};
use crate::providers::base::{
    current_working_dir, ProviderDef, ProviderDescriptor, ProviderMetadata,
};
use crate::providers::catalog::ProviderSetupMetadata;

pub(crate) const ROUTER_ACP_PROVIDER_NAME: &str = "router-acp";
pub(crate) const ROUTER_ACP_BINARY: &str = "router-acp";
const ROUTER_ACP_DOC_URL: &str = "https://github.com/zaneclaes/router-acp";
const ROUTER_ACP_CONFIG_KEY: &str = "ROUTER_ACP_CONFIG";

pub struct RouterAcpProvider;

impl goose_providers::base::ProviderDescriptor for RouterAcpProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata::new(
            ROUTER_ACP_PROVIDER_NAME,
            "Router",
            "Route each session across agents and models with router-acp.",
            ACP_CURRENT_MODEL,
            vec![],
            ROUTER_ACP_DOC_URL,
            vec![],
        )
        .with_setup_steps(vec![
            "Install router-acp and connect its provider accounts",
            "Use .router-acp/router.yaml for project overrides, or set ROUTER_ACP_CONFIG to select a specific file",
        ])
        .with_setup(
            ProviderSetupMetadata::cli_agent(ROUTER_ACP_BINARY, &["router-acp"])
                .with_acp()
                .with_docs_url(ROUTER_ACP_DOC_URL)
                .show_only_when_installed(),
        )
    }
}

/// The router owns project discovery, precedence and environment expansion.
/// Hosts use its selected path for both ACP startup and read-only state queries.
fn router_config_path(config: &Config, binary: &Path, cwd: &Path) -> Result<PathBuf> {
    let explicit = match config.get_param::<String>(ROUTER_ACP_CONFIG_KEY) {
        Ok(value) => Some(value),
        Err(ConfigError::NotFound(_)) => None,
        Err(error) => return Err(error.into()),
    };
    let mut command = std::process::Command::new(binary);
    command
        .args(["config-path", "--cwd"])
        .arg(cwd)
        .stdin(Stdio::null());
    if let Some(path) = explicit {
        command.arg("--config").arg(path);
    }
    let output = command.output()?;
    anyhow::ensure!(output.status.success(),
        "router-acp could not resolve its configuration; install a version supporting config-path and check ROUTER_ACP_CONFIG");
    let selected = String::from_utf8(output.stdout)?;
    let selected = PathBuf::from(selected.trim());
    anyhow::ensure!(
        selected.is_absolute(),
        "router-acp returned a non-absolute configuration path"
    );
    Ok(selected)
}

pub fn is_router_acp(provider_name: &str) -> bool {
    provider_name == ROUTER_ACP_PROVIDER_NAME
}

/// Where router-acp keeps the state a router session's delegates live in: the binary the
/// provider spawns and the config it spawns it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterStateSource {
    pub binary: PathBuf,
    pub config: PathBuf,
}

impl RouterStateSource {
    pub fn configured() -> Result<Self> {
        Self::configured_for(&current_working_dir())
    }

    fn configured_for(cwd: &Path) -> Result<Self> {
        let binary = SearchPaths::builder()
            .with_npm()
            .resolve(ROUTER_ACP_BINARY)?;
        let config = router_config_path(Config::global(), &binary, cwd)?;
        Ok(Self { binary, config })
    }

    /// Delegate panels of every given router session, from router-acp's read-only
    /// `state-query v1 delegates` boundary.
    pub async fn delegates(
        &self,
        parent_session_ids: &[String],
        timeout: Duration,
    ) -> Result<Vec<RouterDelegate>> {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .arg("state-query")
            .arg("--config")
            .arg(&self.config)
            .args(["v1", "delegates"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for id in parent_session_ids {
            command.arg("--session").arg(id);
        }
        crate::subprocess::configure_subprocess(&mut command);
        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| anyhow::anyhow!("router-acp state-query timed out"))??;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr.trim().lines().last().unwrap_or("no error output");
            anyhow::bail!("router-acp state-query failed: {reason}");
        }
        parse_delegates(&output.stdout)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterDelegate {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub has_response: bool,
    /// The planner work this delegate runs, when router-acp's planner started it.
    #[serde(default)]
    pub work_id: Option<String>,
    /// A planner worker's lifecycle from router-acp's durable work record: `running`
    /// while its attempt is open, else the work's status (`review-pending`, `finished`,
    /// `interrupted`, ...), or `ended` for a session a later one replaced.
    #[serde(default)]
    pub state: Option<String>,
    pub routing: Option<serde_json::Value>,
    #[serde(default)]
    pub log: Vec<RouterDelegateLog>,
}

impl RouterDelegate {
    /// The router session that delegated this one.
    pub fn parent_id(&self) -> Option<&str> {
        self.routing
            .as_ref()
            .and_then(|routing| routing.get("parent"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| self.id.rsplit_once(DELEGATE_ID_SEPARATOR).map(|(p, _)| p))
    }
}

/// router-acp names a delegate session `<parent router id>::delegate-<downstream id>`.
pub const DELEGATE_ID_SEPARATOR: &str = "::delegate-";

#[derive(Debug, Clone, Deserialize)]
pub struct RouterDelegateLog {
    pub ts: Option<DateTime<Utc>>,
    pub kind: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub summary: String,
    pub detail: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StateQueryEnvelope {
    version: u32,
    command: String,
    data: DelegatesData,
}

#[derive(Deserialize)]
struct DelegatesData {
    children: Vec<RouterDelegate>,
}

pub fn parse_delegates(json: &[u8]) -> Result<Vec<RouterDelegate>> {
    let envelope: StateQueryEnvelope = serde_json::from_slice(json)?;
    if envelope.version != 1 || envelope.command != "delegates" {
        anyhow::bail!(
            "unexpected router-acp state-query reply: version {} {}",
            envelope.version,
            envelope.command
        );
    }
    Ok(envelope.data.children)
}

impl RouterAcpProvider {
    fn create(
        extensions: Vec<crate::config::ExtensionConfig>,
        working_dir: PathBuf,
        use_default_model: bool,
    ) -> BoxFuture<'static, Result<AcpProvider>> {
        Box::pin(async move {
            let config = Config::global();
            let RouterStateSource {
                binary: resolved_command,
                config: router_config,
            } = RouterStateSource::configured_for(&working_dir)?;
            let goose_mode = config.resolve_goose_mode()?;
            let model = if use_default_model {
                ACP_CURRENT_MODEL.to_string()
            } else {
                configured_model_for_provider(config, ROUTER_ACP_PROVIDER_NAME)
            };

            let session_config_options = if model == ACP_CURRENT_MODEL {
                vec![]
            } else {
                vec![("model".to_string(), model)]
            };

            let provider_config = AcpProviderConfig {
                command: resolved_command,
                args: vec![
                    "serve".to_string(),
                    "--config".to_string(),
                    router_config.display().to_string(),
                ],
                env: vec![],
                env_remove: vec![],
                work_dir: working_dir,
                mcp_servers: extension_configs_to_mcp_servers(&extensions),
                session_mode_id: None,
                session_config_options,
                model_config_option_id: Some("model".to_string()),
                mode_mapping: HashMap::new(),
                notification_callback: None,
            };

            let metadata = Self::metadata();
            AcpProvider::connect(metadata.name, goose_mode, provider_config).await
        })
    }
}

impl ProviderDef for RouterAcpProvider {
    type Provider = AcpProvider;
    const SESSION_BOUND: bool = true;
    const RUNS_OWN_TOOL_LOOP: bool = true;

    fn from_env(
        extensions: Vec<crate::config::ExtensionConfig>,
        tls_config: Option<crate::providers::api_client::TlsConfig>,
    ) -> BoxFuture<'static, Result<AcpProvider>> {
        Self::from_env_with_working_dir(extensions, current_working_dir(), tls_config)
    }

    fn from_env_with_working_dir(
        extensions: Vec<crate::config::ExtensionConfig>,
        working_dir: PathBuf,
        _tls_config: Option<crate::providers::api_client::TlsConfig>,
    ) -> BoxFuture<'static, Result<AcpProvider>> {
        Self::create(extensions, working_dir, false)
    }

    fn from_env_with_default_model(
        extensions: Vec<crate::config::ExtensionConfig>,
        _tls_config: Option<crate::providers::api_client::TlsConfig>,
    ) -> BoxFuture<'static, Result<AcpProvider>> {
        Self::create(extensions, current_working_dir(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegates_reply_is_read_only_from_a_v1_delegates_envelope() {
        let reply = br#"{"version":1,"command":"delegates","data":{"children":[{
            "id":"rtr-1::delegate-d1","title":"t","agent":null,"model":"opus",
            "createdAt":"2026-10-13T20:32:10Z","updatedAt":"2026-10-13T20:40:19Z",
            "tokensTotal":1,"contextUsed":2,"routing":null,"hasResponse":false,
            "log":[{"ts":"2026-10-13T20:32:10Z","kind":"delegate_task","role":"user",
                    "summary":"task","detail":{"work_id":"w"}}]}]}}"#;
        let delegates = parse_delegates(reply).unwrap();
        assert_eq!(delegates[0].parent_id(), Some("rtr-1"));
        assert_eq!(delegates[0].log[0].kind, "delegate_task");

        let other = br#"{"version":2,"command":"delegates","data":{"children":[]}}"#;
        assert!(parse_delegates(other).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn router_config_uses_native_discovery_and_forwards_explicit_overrides() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = env_lock::lock_env([(ROUTER_ACP_CONFIG_KEY, None::<&str>)]);
        let directory = tempfile::tempdir().unwrap();
        let config = Config::new(directory.path().join("config.yaml"), "test").unwrap();
        let binary = directory.path().join("router-acp");
        let cwd = directory.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(&binary, "#!/bin/sh\n[ \"$1\" = config-path ] && [ \"$2\" = --cwd ] || exit 1\nif [ \"$4\" = --config ]; then printf '%s/explicit.yaml\\n' \"$3\"; else printf '%s/.router-acp/router.yaml\\n' \"$3\"; fi\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            router_config_path(&config, &binary, &cwd).unwrap(),
            cwd.join(".router-acp/router.yaml")
        );

        config
            .set_param(ROUTER_ACP_CONFIG_KEY, "~/routers/kory.yaml")
            .unwrap();
        assert_eq!(
            router_config_path(&config, &binary, &cwd).unwrap(),
            cwd.join("explicit.yaml")
        );
        std::fs::write(&binary, "#!/bin/sh\nexit 1\n").unwrap();
        assert!(router_config_path(&config, &binary, &cwd).is_err());
    }
}
