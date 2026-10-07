//! Local provider setup, independent of OpenAI account authentication.

use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use codex_login::provider_credentials::read_provider_key;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;

use crate::legacy_core::config::Config;

const DEFAULT_CONFIG: &str = include_str!("../../../../config.example.toml");

/// Ship the starter catalog inside the binary so first launch needs no checkout.
/// Preserve configured catalogs; add missing defaults to preference-only files.
pub(crate) fn initialize_default_config(home: &Path) -> io::Result<()> {
    let path = home.join("config.toml");
    let existing = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => return Ok(()),
        Ok(_) => Some(std::fs::read_to_string(&path)?),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };
    let contents = if let Some(existing) = &existing {
        let Ok(mut document) = existing.parse::<toml_edit::DocumentMut>() else {
            // Leave malformed configuration for the normal loader to diagnose.
            return Ok(());
        };
        if [
            "model",
            "model_provider",
            "model_providers",
            "profile",
            "profiles",
            "forced_login_method",
        ]
        .iter()
        .any(|key| document.contains_key(key))
        {
            return Ok(());
        }
        let defaults = DEFAULT_CONFIG
            .parse::<toml_edit::DocumentMut>()
            .map_err(io::Error::other)?;
        for (key, value) in defaults.iter() {
            if !document.contains_key(key) {
                document.insert(key, value.clone());
            }
        }
        document.to_string()
    } else {
        DEFAULT_CONFIG.to_owned()
    };
    std::fs::create_dir_all(home)?;
    let mut file = tempfile::NamedTempFile::new_in(home)?;
    file.write_all(contents.as_bytes())?;
    if existing.is_some() {
        file.as_file()
            .set_permissions(std::fs::metadata(&path)?.permissions())?;
    }
    file.as_file().sync_all()?;
    if let Some(existing) = existing {
        // Do not replace edits made while preparing the initial configuration.
        if std::fs::read_to_string(&path)? != existing {
            return Ok(());
        }
        return file.persist(path).map(|_| ()).map_err(|err| err.error);
    }
    match file.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.error),
    }
}

#[derive(Clone)]
pub(crate) struct ProviderKeySetup {
    pub provider_name: String,
    pub env_key: String,
    pub codex_home: PathBuf,
}

pub(crate) enum ProviderStartup {
    NeedsConfiguration,
    MissingKey(ProviderKeySetup),
    Ready,
    AccountLogin,
}

pub(crate) fn provider_startup(
    config: &Config,
    explicit_provider_override: bool,
) -> io::Result<ProviderStartup> {
    if !explicit_provider_override
        && config.model_provider_id == "openai"
        && config
            .config_layer_stack
            .effective_config()
            .get("model_provider")
            .is_none()
        && config
            .config_layer_stack
            .required_model_provider()
            .is_none()
    {
        return Ok(ProviderStartup::NeedsConfiguration);
    }
    if let Some(env_key) = &config.model_provider.env_key {
        if read_provider_key(&config.codex_home, env_key)?.is_none() {
            return Ok(ProviderStartup::MissingKey(ProviderKeySetup {
                provider_name: config.model_provider.name.clone(),
                env_key: env_key.clone(),
                codex_home: config.codex_home.to_path_buf(),
            }));
        }
        return Ok(ProviderStartup::Ready);
    }
    if config.model_provider.requires_openai_auth {
        Ok(ProviderStartup::AccountLogin)
    } else {
        Ok(ProviderStartup::Ready)
    }
}

impl ProviderKeySetup {
    pub(crate) fn render_entry(
        &self,
        value: &str,
        error: Option<String>,
        area: Rect,
        buf: &mut Buffer,
    ) {
        let mut lines = vec![
            Line::from(format!("Enter your {} API key", self.provider_name).bold()),
            Line::default(),
            Line::from(format!("Save {} in:", self.env_key)),
            Line::from(self.codex_home.join(".env").display().to_string()),
            Line::default(),
            Line::from(if value.is_empty() {
                "Key: paste or type here"
            } else {
                "Key: ********"
            }),
            Line::default(),
            Line::from("Enter: save and continue · Esc: cancel"),
        ];
        if let Some(error) = error {
            lines.push(Line::default());
            lines.push(Line::from(error.red()));
        }
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy_core::config::ConfigBuilder;
    use codex_config::LoaderOverrides;
    use codex_login::provider_credentials::save_provider_key;
    use pretty_assertions::assert_eq;

    async fn config(home: &std::path::Path) -> Config {
        ConfigBuilder::default()
            .codex_home(home.to_path_buf())
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn local_startup_uses_selected_provider_and_saved_key() {
        let home = tempfile::tempdir().unwrap();
        assert!(matches!(
            provider_startup(&config(home.path()).await, false).unwrap(),
            ProviderStartup::NeedsConfiguration
        ));
        std::fs::write(
            home.path().join("config.toml"),
            r#"
model_provider = "openrouter"
[model_providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
env_key = "CUSTOMCODEX_STARTUP_TEST_KEY_8132"
[model_providers.unused]
name = "Unused provider"
env_key = "CUSTOMCODEX_UNUSED_TEST_KEY_8132"
"#,
        )
        .unwrap();
        let config = config(home.path()).await;
        let ProviderStartup::MissingKey(setup) = provider_startup(&config, false).unwrap() else {
            panic!("expected key setup")
        };
        assert_eq!(setup.provider_name, "OpenRouter");
        assert_eq!(setup.env_key, "CUSTOMCODEX_STARTUP_TEST_KEY_8132");
        save_provider_key(home.path(), &setup.env_key, "test-secret").unwrap();
        assert!(matches!(
            provider_startup(&config, false).unwrap(),
            ProviderStartup::Ready
        ));
        assert!(!home.path().join("auth.json").exists());
    }

    #[tokio::test]
    async fn explicit_openai_and_keyless_providers_keep_their_auth_flow() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "model_provider = 'openai'\n",
        )
        .unwrap();
        assert!(matches!(
            provider_startup(&config(home.path()).await, false).unwrap(),
            ProviderStartup::AccountLogin
        ));
        std::fs::write(
            home.path().join("config.toml"),
            "model_provider = 'ollama'\n",
        )
        .unwrap();
        assert!(matches!(
            provider_startup(&config(home.path()).await, false).unwrap(),
            ProviderStartup::Ready
        ));
    }

    #[tokio::test]
    async fn provider_template_populates_existing_catalog() {
        let home = tempfile::tempdir().unwrap();
        initialize_default_config(home.path()).unwrap();
        let config = config(home.path()).await;
        assert_eq!(config.model_provider_id, "openrouter");
        assert_eq!(
            config.model.as_deref(),
            Some("deepseek/deepseek-v4.1-flash")
        );
        assert_eq!(
            config.model_provider.env_key.as_deref(),
            Some("OPENROUTER_API_KEY")
        );
        assert!(!config.model_provider.supports_websockets);
        let model = &config.model_provider.models[0];
        assert_eq!(model.id, "deepseek/deepseek-v4.1-flash");
        assert_eq!(model.context_window, 1048576);
        assert_eq!(
            model.reasoning_effort,
            Some(codex_protocol::openai_models::ReasoningEffort::Max)
        );
        assert_eq!(model.openrouter_providers, vec!["together"]);
        assert_eq!(model.openrouter_zdr, Some(true));
        assert_eq!(config.model_reasoning_effort, model.reasoning_effort);
    }

    #[test]
    fn default_config_preserves_existing_files() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        for existing in [
            "model_provider = 'ollama'\n",
            "model = 'custom'\n",
            "[model_providers.custom]\nname = 'Custom'\n",
            "[profiles.custom]\nmodel_provider = 'ollama'\n",
            "invalid toml [",
        ] {
            std::fs::write(&path, existing).unwrap();
            initialize_default_config(home.path()).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);
        }
    }

    #[tokio::test]
    async fn default_config_merges_preferences_and_loads_provider() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        for existing in [
            "",
            "# Keep preferences\nweb_search = 'cached'\n[tui]\nanimations = false # Keep comment\n",
        ] {
            std::fs::write(&path, existing).unwrap();
            initialize_default_config(home.path()).unwrap();
            let contents = std::fs::read_to_string(&path).unwrap();
            let merged: toml::Value = toml::from_str(&contents).unwrap();
            let original: toml::Value = toml::from_str(existing).unwrap();
            for (key, value) in original.as_table().unwrap() {
                assert_eq!(&merged[key], value);
            }
            if !existing.is_empty() {
                assert!(contents.contains("# Keep preferences"));
                assert!(contents.contains("# Keep comment"));
            }
            assert_eq!(config(home.path()).await.model_provider_id, "openrouter");
            initialize_default_config(home.path()).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        }
    }

    #[test]
    fn default_config_is_idempotent_and_creates_missing_directory() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("new-home");
        initialize_default_config(&home).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            DEFAULT_CONFIG
        );
        initialize_default_config(&home).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            DEFAULT_CONFIG
        );
        assert!(!home.join(".env").exists());
    }

    #[cfg(unix)]
    #[test]
    fn default_config_preserves_dangling_symlink() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("missing-target");
        let path = root.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        initialize_default_config(root.path()).unwrap();
        assert_eq!(std::fs::read_link(path).unwrap(), target);
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn provider_startup_honors_cli_override() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            r#"
model_provider = "missing-key"
[model_providers.missing-key]
name = "Missing key"
env_key = "CUSTOMCODEX_UNUSED_OVERRIDE_KEY_4910"
"#,
        )
        .unwrap();
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .cli_overrides(vec![(
                "model_provider".into(),
                toml::Value::String("ollama".into()),
            )])
            .build()
            .await
            .unwrap();
        assert!(matches!(
            provider_startup(&config, false).unwrap(),
            ProviderStartup::Ready
        ));
    }

    #[test]
    fn provider_key_entry_masks_secrets_at_narrow_widths() {
        let setup = ProviderKeySetup {
            provider_name: "OpenRouter".into(),
            env_key: "OPENROUTER_API_KEY".into(),
            codex_home: PathBuf::from("/home/user/.customcodex"),
        };
        for width in [32, 80] {
            let area = Rect::new(0, 0, width, 18);
            let mut buffer = Buffer::empty(area);
            setup.render_entry("sk-secret-not-visible", None, area, &mut buffer);
            let rendered = (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!rendered.contains("sk-secret"));
            assert!(rendered.contains("********"));
            insta::assert_snapshot!(format!("provider_key_entry_{width}"), rendered);
        }
    }
}
