use crate::installed_marketplaces::marketplace_install_root;
use crate::marketplace_policy::validate_marketplace_name_for_add;
use crate::marketplace_policy::validate_marketplace_source_for_add;
use codex_config::ConfigRequirements;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;
use std::path::PathBuf;

mod metadata;
mod source;

use metadata::MarketplaceInstallMetadata;
use metadata::find_marketplace_root_by_name;
use metadata::installed_marketplace_root_for_source;
use metadata::record_added_marketplace_entry;
pub(crate) use source::MarketplaceSource;
pub(crate) use source::parse_marketplace_source;
use source::validate_marketplace_source_root;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceAddRequest {
    pub source: String,
    pub ref_name: Option<String>,
    pub sparse_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceAddOutcome {
    pub marketplace_name: String,
    pub source_display: String,
    pub installed_root: AbsolutePathBuf,
    pub already_added: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MarketplaceAddError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{0}")]
    Internal(String),
}

pub async fn add_marketplace(
    codex_home: PathBuf,
    requirements: ConfigRequirements,
    request: MarketplaceAddRequest,
) -> Result<MarketplaceAddOutcome, MarketplaceAddError> {
    tokio::task::spawn_blocking(move || {
        add_marketplace_sync(codex_home.as_path(), &requirements, request)
    })
    .await
    .map_err(|err| MarketplaceAddError::Internal(format!("failed to add marketplace: {err}")))?
}

pub fn is_local_marketplace_source(
    source: &str,
    explicit_ref: Option<String>,
) -> Result<bool, MarketplaceAddError> {
    Ok(matches!(
        parse_marketplace_source(source, explicit_ref)?,
        source::MarketplaceSource::Local { .. }
    ))
}

fn add_marketplace_sync(
    codex_home: &Path,
    requirements: &ConfigRequirements,
    request: MarketplaceAddRequest,
) -> Result<MarketplaceAddOutcome, MarketplaceAddError> {
    let source = parse_marketplace_source(&request.source, request.ref_name)?;
    let MarketplaceSource::Local { path } = &source else {
        return Err(MarketplaceAddError::InvalidRequest(
            "Online marketplaces are unsupported; provide a local marketplace directory."
                .to_string(),
        ));
    };
    if !request.sparse_paths.is_empty() {
        return Err(MarketplaceAddError::InvalidRequest(
            "--sparse is unsupported for local marketplaces".to_string(),
        ));
    }
    let managed_name = validate_marketplace_source_for_add(codex_home, requirements, &source)
        .map_err(MarketplaceAddError::InvalidRequest)?;
    let name = validate_marketplace_source_root(path)?;
    validate_marketplace_name_for_add(managed_name, &name)
        .map_err(MarketplaceAddError::InvalidRequest)?;
    let install_root = marketplace_install_root(codex_home);
    let metadata = MarketplaceInstallMetadata::from_source(&source, &[]);
    let already_added =
        installed_marketplace_root_for_source(codex_home, &install_root, &metadata)?.is_some();
    if !already_added && find_marketplace_root_by_name(codex_home, &install_root, &name)?.is_some()
    {
        return Err(MarketplaceAddError::InvalidRequest(format!(
            "marketplace '{name}' is already added from another source"
        )));
    }
    record_added_marketplace_entry(codex_home, &name, &metadata)?;
    Ok(MarketplaceAddOutcome {
        marketplace_name: name,
        source_display: source.display(),
        installed_root: AbsolutePathBuf::try_from(path.clone())
            .map_err(|error| MarketplaceAddError::Internal(error.to_string()))?,
        already_added,
    })
}
