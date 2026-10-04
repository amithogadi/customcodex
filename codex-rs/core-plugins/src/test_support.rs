use crate::{PluginsConfigInput, PluginsManager};
use codex_config::loader::load_config_layers_state;
use codex_config::{LoaderOverrides, NoopThreadConfigLoader};
use codex_exec_server::LOCAL_FS;
use codex_protocol::protocol::SkillScope;
use codex_skills::{
    LoadedSkillRoot, LoadedSkills, SkillError, SkillLoadFuture, SkillMetadata,
    SkillRootLoadRequest, SkillRootLoader, parse_skill_frontmatter_metadata,
};
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_plugins::{PluginSkillRoot, SkillDiscoveryMode, migrated_command_skills_root};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
pub(crate) fn test_plugins_manager(codex_home: PathBuf) -> PluginsManager {
    PluginsManager::new(codex_home, test_skill_root_loader())
}
pub(crate) fn test_skill_root_loader() -> Arc<dyn SkillRootLoader<PluginSkillRoot>> {
    Arc::new(TestSkillRootLoader)
}

struct TestSkillRootLoader;

impl SkillRootLoader<PluginSkillRoot> for TestSkillRootLoader {
    fn load_roots(
        &self,
        request: SkillRootLoadRequest<PluginSkillRoot>,
    ) -> SkillLoadFuture<'_, LoadedSkills> {
        Box::pin(async move {
            let mut loaded_roots = Vec::new();
            for root in request.roots {
                let cached = request
                    .snapshots
                    .as_ref()
                    .and_then(|cache| cache.get(&root));
                let snapshot = match cached {
                    Some(snapshot) => snapshot,
                    None => {
                        let snapshot = load_test_skill_root(&root);
                        if let Some(snapshots) = &request.snapshots {
                            snapshots.insert(root.clone(), snapshot.clone());
                        }
                        snapshot
                    }
                };
                let migrated_root = migrated_command_skills_root(&root.plugin_root);
                let canonical_migrated_root = fs::canonicalize(migrated_root.as_path())
                    .ok()
                    .and_then(|path| AbsolutePathBuf::from_absolute_path_checked(path).ok())
                    .unwrap_or(migrated_root);
                loaded_roots.push((snapshot.root == canonical_migrated_root, snapshot));
            }

            let native_names = loaded_roots
                .iter()
                .filter(|(migrated, _)| !migrated)
                .flat_map(|(_, snapshot)| &snapshot.skills)
                .map(|skill| (skill.plugin_id.clone(), skill.name.clone()))
                .collect::<HashSet<_>>();
            let mut seen_paths = HashSet::new();
            let mut outcome = LoadedSkills::default();
            for (migrated, snapshot) in loaded_roots {
                outcome
                    .skills
                    .extend(snapshot.skills.into_iter().filter(|skill| {
                        (!migrated
                            || !native_names
                                .contains(&(skill.plugin_id.clone(), skill.name.clone())))
                            && skill.matches_product_restriction_for_product(
                                request.restriction_product,
                            )
                            && seen_paths.insert(skill.path_to_skills_md.clone())
                    }));
                outcome.errors.extend(snapshot.errors);
            }
            outcome.skills.sort_by(|left, right| {
                left.name
                    .cmp(&right.name)
                    .then_with(|| left.path_to_skills_md.cmp(&right.path_to_skills_md))
            });
            outcome
        })
    }
}

fn load_test_skill_root(root: &PluginSkillRoot) -> LoadedSkillRoot {
    let canonical_root = fs::canonicalize(root.path.as_path())
        .ok()
        .and_then(|path| AbsolutePathBuf::from_absolute_path_checked(path).ok())
        .unwrap_or_else(|| root.path.clone());
    let mut skills = Vec::new();
    let mut errors = Vec::new();
    let mut discovery_paths = HashMap::new();
    let mut directories = vec![root.path.clone()];
    while let Some(directory) = directories.pop() {
        let Ok(entries) = fs::read_dir(directory.as_path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if (root.discovery_mode == SkillDiscoveryMode::Recursive || directory == root.path)
                    && let Ok(path) = AbsolutePathBuf::from_absolute_path_checked(path)
                {
                    directories.push(path);
                }
                continue;
            }
            if path.file_name().is_none_or(|name| name != "SKILL.md")
                || (root.discovery_mode == SkillDiscoveryMode::DirectChildren
                    && directory == root.path)
            {
                continue;
            }
            let Ok(path) = AbsolutePathBuf::from_absolute_path_checked(path) else {
                continue;
            };
            let canonical_path = fs::canonicalize(path.as_path())
                .ok()
                .and_then(|path| AbsolutePathBuf::from_absolute_path_checked(path).ok())
                .unwrap_or_else(|| path.clone());
            let parsed = fs::read_to_string(path.as_path())
                .map_err(|error| error.to_string())
                .and_then(|contents| {
                    parse_skill_frontmatter_metadata(&contents, || {
                        directory
                            .as_path()
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_string()
                    })
                    .map_err(|error| error.to_string())
                });
            match parsed {
                Ok(parsed) => {
                    discovery_paths.insert(canonical_path.clone(), path);
                    skills.push(SkillMetadata {
                        name: format!("{}:{}", root.plugin_namespace, parsed.name),
                        description: parsed.description,
                        short_description: parsed.short_description,
                        interface: None,
                        dependencies: None,
                        policy: None,
                        path_to_skills_md: canonical_path,
                        scope: SkillScope::User,
                        plugin_id: Some(root.plugin_identity.plugin_id.clone()),
                        remote_plugin_id: root.plugin_identity.remote_plugin_id.clone(),
                    });
                }
                Err(message) => errors.push(SkillError {
                    path: canonical_path,
                    message,
                }),
            }
        }
    }

    LoadedSkillRoot {
        root: canonical_root,
        skills,
        skill_discovery_path_by_path: Arc::new(discovery_paths),
        errors,
        is_agent_plugin: root.discovery_mode == SkillDiscoveryMode::DirectChildren,
    }
}

pub(crate) fn write_file(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("file should have a parent")).unwrap();
    fs::write(path, contents).unwrap();
}
pub(crate) async fn load_plugins_config(codex_home: &Path, cwd: &Path) -> PluginsConfigInput {
    let codex_home = AbsolutePathBuf::try_from(codex_home).expect("codex home should be absolute");
    let cwd = AbsolutePathBuf::try_from(cwd).expect("cwd should be absolute");
    let config_layer_stack = load_config_layers_state(
        LOCAL_FS.as_ref(),
        codex_home.as_path(),
        Some(cwd),
        &[],
        LoaderOverrides::without_managed_config_for_tests(),
        &NoopThreadConfigLoader,
    )
    .await
    .expect("config should load");
    let enabled = config_layer_stack
        .effective_config()
        .get("features")
        .and_then(|value| value.get("plugins"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    PluginsConfigInput::new(config_layer_stack, enabled)
}
