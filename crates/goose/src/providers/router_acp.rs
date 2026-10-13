use anyhow::Result;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::acp::{
    configured_model_for_provider, extension_configs_to_mcp_servers, AcpProvider,
    AcpProviderConfig, ACP_CURRENT_MODEL,
};
use crate::config::search_path::SearchPaths;
use crate::config::Config;
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
            "Point ROUTER_ACP_CONFIG at your router.yaml (default: ~/.config/router-acp/router.yaml)",
        ])
        .with_setup(
            ProviderSetupMetadata::cli_agent(ROUTER_ACP_BINARY, &["router-acp"])
                .with_acp()
                .with_docs_url(ROUTER_ACP_DOC_URL)
                .show_only_when_installed(),
        )
    }
}

/// router-acp requires an explicit config; the default matches where router-acp hosts write it.
fn router_config_path(config: &Config) -> Result<PathBuf> {
    if let Ok(path) = config.get_param::<String>(ROUTER_ACP_CONFIG_KEY) {
        return Ok(PathBuf::from(shellexpand::tilde(&path).into_owned()));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Set {ROUTER_ACP_CONFIG_KEY} to your router.yaml"))?;
    Ok(home.join(".config").join("router-acp").join("router.yaml"))
}

impl RouterAcpProvider {
    fn create(
        extensions: Vec<crate::config::ExtensionConfig>,
        working_dir: PathBuf,
        use_default_model: bool,
    ) -> BoxFuture<'static, Result<AcpProvider>> {
        Box::pin(async move {
            let config = Config::global();
            let resolved_command = SearchPaths::builder()
                .with_npm()
                .resolve(ROUTER_ACP_BINARY)?;
            let router_config = router_config_path(config)?;
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
    fn router_config_defaults_to_the_router_acp_home_and_honors_overrides() {
        let _guard = env_lock::lock_env([(ROUTER_ACP_CONFIG_KEY, None::<&str>)]);
        let directory = tempfile::tempdir().unwrap();
        let config = Config::new(directory.path().join("config.yaml"), "test").unwrap();
        let home = dirs::home_dir().unwrap();

        assert_eq!(
            router_config_path(&config).unwrap(),
            home.join(".config/router-acp/router.yaml")
        );

        config
            .set_param(ROUTER_ACP_CONFIG_KEY, "~/routers/kory.yaml")
            .unwrap();
        assert_eq!(
            router_config_path(&config).unwrap(),
            home.join("routers/kory.yaml")
        );
    }
}
