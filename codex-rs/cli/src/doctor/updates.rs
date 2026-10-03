//! Diagnoses whether Codex update paths target the running installation.
//!
//! Update diagnostics combine cached version metadata, install-channel hints,
//! and bounded latest-version HTTP probes. It never executes package managers or
//! other helpers selected by PATH; npm update targets are not verified.

use std::path::Path;
use std::time::Duration;

use codex_core::config::Config;
use codex_http_client::ClientRouteClass;
use codex_http_client::RouteAwareClientPool;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use http::Method;
use serde::Deserialize;

use super::CheckStatus;
use super::DoctorCheck;
use super::doctor_install_context;
use super::doctor_managed_by_npm;

const MAX_VERSION_RESPONSE_BYTES: usize = 1024 * 1024;

const VERSION_FILE_NAME: &str = "version.json";
const GITHUB_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";
const HOMEBREW_CASK_API_URL: &str = "https://formulae.brew.sh/api/cask/codex.json";
/// Builds the update-health row for the current installation.
///
/// Network failures while fetching latest-version metadata degrade the row to a
/// warning instead of failing doctor outright; update freshness is useful
/// support context but should not mask more direct install/config failures.
pub(super) async fn updates_check(config: &Config) -> DoctorCheck {
    let current_exe = std::env::current_exe().ok();
    let install_context = doctor_install_context(current_exe.as_deref());
    let mut details = vec![
        format!(
            "check for update on startup: {}",
            config.check_for_update_on_startup
        ),
        format!("update action: {}", update_action_label(&install_context)),
    ];
    let version_file = config.codex_home.join(VERSION_FILE_NAME);
    push_cached_version_details(&mut details, &version_file);

    let mut status = CheckStatus::Ok;
    let summary = "update configuration is locally consistent".to_string();

    if doctor_managed_by_npm(current_exe.as_deref()) {
        details
            .push("npm update target: not inspected (PATH helpers are not executed)".to_string());
    }
    let client = RouteAwareClientPool::new_without_request_logging(
        config.http_client_factory(),
        ClientRouteClass::Other,
    );

    match fetch_latest_version(&client, &install_context).await {
        Ok(latest_version) => {
            details.push(format!("latest version: {latest_version}"));
            if is_newer(&latest_version, env!("CARGO_PKG_VERSION")) == Some(true) {
                details.push("latest version status: newer version is available".to_string());
            } else {
                details.push("latest version status: current version is not older".to_string());
            }
        }
        Err(err) => {
            status = status.max(CheckStatus::Warning);
            details.push(format!("latest version probe: {err}"));
        }
    }

    DoctorCheck::new("updates.status", "updates", status, summary).details(details)
}

fn push_cached_version_details(details: &mut Vec<String>, version_file: &Path) {
    details.push(format!("version cache: {}", version_file.display()));
    match std::fs::read_to_string(version_file) {
        Ok(contents) => match serde_json::from_str::<VersionInfo>(&contents) {
            Ok(info) => {
                details.push(format!("cached latest version: {}", info.latest_version));
                if let Some(last_checked_at) = info.last_checked_at {
                    details.push(format!("last checked at: {last_checked_at}"));
                }
                if let Some(dismissed_version) = info.dismissed_version {
                    details.push(format!("dismissed version: {dismissed_version}"));
                }
            }
            Err(err) => details.push(format!("version cache parse: {err}")),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            details.push("version cache: missing".to_string());
        }
        Err(err) => details.push(format!("version cache read: {err}")),
    }
}

fn update_action_label(context: &InstallContext) -> &'static str {
    match &context.method {
        InstallMethod::Npm => "npm install -g @openai/codex",
        InstallMethod::Bun => "bun install -g @openai/codex",
        InstallMethod::VitePlus => "vp install -g @openai/codex",
        InstallMethod::Pnpm => "pnpm add -g @openai/codex",
        InstallMethod::Brew => "brew upgrade --cask codex",
        InstallMethod::Standalone { .. } => "standalone installer",
        InstallMethod::Other => "manual or unknown",
    }
}

async fn fetch_latest_version(
    client: &RouteAwareClientPool,
    context: &InstallContext,
) -> Result<String, String> {
    match &context.method {
        InstallMethod::Brew => fetch_homebrew_cask_version(client).await,
        InstallMethod::Npm
        | InstallMethod::Bun
        | InstallMethod::VitePlus
        | InstallMethod::Pnpm
        | InstallMethod::Standalone { .. }
        | InstallMethod::Other => fetch_latest_github_release_version(client).await,
    }
}

async fn fetch_latest_github_release_version(
    client: &RouteAwareClientPool,
) -> Result<String, String> {
    #[derive(Deserialize)]
    struct ReleaseInfo {
        tag_name: String,
    }

    let info = http_get_json::<ReleaseInfo>(client, GITHUB_LATEST_RELEASE_URL).await?;
    info.tag_name
        .strip_prefix("rust-v")
        .map(str::to_string)
        .ok_or_else(|| format!("failed to parse latest tag {}", info.tag_name))
}

async fn fetch_homebrew_cask_version(client: &RouteAwareClientPool) -> Result<String, String> {
    #[derive(Deserialize)]
    struct HomebrewCaskInfo {
        version: String,
    }

    http_get_json::<HomebrewCaskInfo>(client, HOMEBREW_CASK_API_URL)
        .await
        .map(|info| info.version)
}

async fn http_get_json<T>(client: &RouteAwareClientPool, url: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        let mut response = client
            .request(Method::GET, url)
            .header(
                http::header::USER_AGENT,
                concat!("codex-doctor/", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await
            .map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|err| err.to_string())? {
            if chunk.len() > MAX_VERSION_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err("version response exceeds size limit".to_string());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|err| err.to_string())
    })
    .await
    .map_err(|_| "version request timed out".to_string())?
}

fn is_newer(latest: &str, current: &str) -> Option<bool> {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => Some(latest > current),
        (Some(_), None) | (None, Some(_)) | (None, None) => None,
    }
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let mut parts = value.trim().split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    Some((major, minor, patch))
}

#[derive(Deserialize)]
struct VersionInfo {
    latest_version: String,
    #[serde(default)]
    last_checked_at: Option<String>,
    #[serde(default)]
    dismissed_version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn version_http_probe_decodes_json_and_rejects_invalid_responses() {
        use codex_http_client::HttpClientFactory;
        use codex_http_client::OutboundProxyPolicy;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        let client = RouteAwareClientPool::new_without_request_logging(
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            ClientRouteClass::Other,
        );
        for (endpoint, response) in [
            (
                "valid",
                ResponseTemplate::new(/*s*/ 200)
                    .set_body_json(serde_json::json!({"version": "1.2.3"})),
            ),
            (
                "invalid",
                ResponseTemplate::new(/*s*/ 200).set_body_string("not JSON"),
            ),
            ("unavailable", ResponseTemplate::new(/*s*/ 503)),
            ("proxy_auth_required", ResponseTemplate::new(/*s*/ 407)),
            (
                "redirect",
                ResponseTemplate::new(/*s*/ 302)
                    .insert_header("Location", format!("{}/valid", server.uri())),
            ),
            (
                "timeout",
                ResponseTemplate::new(/*s*/ 200).set_delay(Duration::from_secs(/*secs*/ 6)),
            ),
            (
                "oversized",
                ResponseTemplate::new(/*s*/ 200)
                    .set_body_bytes(vec![b' '; MAX_VERSION_RESPONSE_BYTES + 1]),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/{endpoint}")))
                .respond_with(response)
                .mount(&server)
                .await;
            let result = http_get_json::<serde_json::Value>(
                &client,
                &format!("{}/{endpoint}", server.uri()),
            )
            .await;
            if matches!(endpoint, "valid" | "redirect") {
                assert_eq!(result, Ok(serde_json::json!({"version": "1.2.3"})));
            } else if endpoint == "timeout" {
                assert_eq!(result, Err("version request timed out".to_string()));
            } else if endpoint == "proxy_auth_required" {
                assert_eq!(
                    result,
                    Err("HTTP 407 Proxy Authentication Required".to_string())
                );
            } else {
                assert!(result.is_err(), "{endpoint} must not be accepted");
            }
        }
    }





    #[test]
    fn is_newer_compares_plain_semver() {
        assert_eq!(is_newer("1.2.4", "1.2.3"), Some(true));
        assert_eq!(is_newer("1.2.3", "1.2.4"), Some(false));
        assert_eq!(is_newer("1.2.3-beta.1", "1.2.2"), None);
    }

    #[test]
    fn update_action_labels_install_contexts() {
        assert_eq!(
            update_action_label(&InstallContext {
                method: InstallMethod::Npm,
                package_layout: None,
            }),
            "npm install -g @openai/codex"
        );
        assert_eq!(
            update_action_label(&InstallContext {
                method: InstallMethod::Pnpm,
                package_layout: None,
            }),
            "pnpm add -g @openai/codex"
        );
        assert_eq!(
            update_action_label(&InstallContext {
                method: InstallMethod::Other,
                package_layout: None,
            }),
            "manual or unknown"
        );
    }
}
