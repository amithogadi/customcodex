//! Managed app-server lifecycle, serialized across CLI invocations and the updater.

mod backend;
mod background_command;
#[cfg(windows)]
pub use backend::windows::DetachedLaunchRestricted;
#[cfg(windows)]
pub use backend::windows::is_elevated;
#[cfg(windows)]
use backend::windows::try_lock_file;
mod client;
mod diagnostics;
mod install_lock;
mod launch;
pub use launch::restart_with_features;
pub use launch::start_with_features;
mod managed_install;
mod prepare_install;
pub use prepare_install::InstallRequest;
mod settings;
mod thread_recovery;

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
pub use backend::BackendKind;
use backend::BackendPaths;
use codex_app_server_transport::app_server_control_socket_path;
use codex_utils_home_dir::find_codex_home;
use managed_install::managed_codex_bin;
#[cfg(any(unix, windows))]
use managed_install::managed_codex_version;
use serde::Serialize;
use settings::DaemonSettings;
use settings::MAX_SHUTDOWN_GRACE_SECONDS;
use tokio::time::sleep;

const START_POLL_INTERVAL: Duration = Duration::from_millis(50);
const START_TIMEOUT: Duration = Duration::from_secs(10);
// Leave room for the longest graceful stop, forced-exit check, and restart.
const OPERATION_LOCK_TIMEOUT: Duration =
    Duration::from_secs(MAX_SHUTDOWN_GRACE_SECONDS as u64 + 75);
const LEGACY_PID_FILE_NAME: &str = "app-server.pid";
const DAEMON_PID_FILE_NAME: &str = "daemon.pid";
const OPERATION_LOCK_FILE_NAME: &str = "daemon.lock";
const SETTINGS_FILE_NAME: &str = "settings.json";
const STATE_DIR_NAME: &str = "app-server-daemon";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleCommand {
    Start,
    Restart,
    Stop,
    Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LifecycleStatus {
    AlreadyRunning,
    Started,
    Restarted,
    Stopped,
    NotRunning,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleOutput {
    pub status: LifecycleStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub managed_codex_path: PathBuf,
    pub managed_codex_version: Option<String>,
    pub socket_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_server_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapOptions {}

/// Passively probes an existing app-server socket and returns its reported
/// app-server version.
pub async fn probe_app_server_version(socket_path: &Path) -> Result<String> {
    Ok(client::probe(socket_path).await?.app_server_version)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BootstrapStatus {
    Bootstrapped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapOutput {
    pub status: BootstrapStatus,
    pub backend: BackendKind,
    pub managed_codex_path: PathBuf,
    pub managed_codex_version: Option<String>,
    pub socket_path: PathBuf,
    pub cli_version: String,
    pub app_server_version: String,
}

#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartIfRunningOutcome {
    Busy,
    NotRunning,
    NotReady,
    AlreadyCurrent,
    Restarted,
}

#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartMode {
    IfVersionChanged,
    IfBinaryOrVersionChanged,
    Always,
}

#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartDecision {
    NotReady,
    AlreadyCurrent,
    Restart,
}

pub async fn run(command: LifecycleCommand) -> Result<LifecycleOutput> {
    ensure_supported_platform()?;
    #[cfg(windows)]
    if matches!(command, LifecycleCommand::Start | LifecycleCommand::Restart) {
        backend::windows::ensure_not_elevated()?;
    }
    // Keep daemon package preparation off callers' async stack frames.
    Box::pin(Daemon::from_environment()?.run(command)).await
}

pub async fn bootstrap(options: BootstrapOptions) -> Result<BootstrapOutput> {
    ensure_supported_platform()?;
    #[cfg(windows)]
    backend::windows::ensure_not_elevated()?;
    Box::pin(Daemon::from_environment()?.bootstrap(options)).await
}

#[cfg(any(unix, windows))]
fn ensure_supported_platform() -> Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn ensure_supported_platform() -> Result<()> {
    Err(anyhow!(
        "codex app-server daemon lifecycle is only supported on Unix and Windows platforms"
    ))
}

#[derive(Clone)]
struct Daemon {
    // Feature-aware TUI startup owns a live terminal. Direct lifecycle commands
    // must still report their diagnostics to stderr.
    log_diagnostics: bool,
    socket_path: PathBuf,
    pid_file: PathBuf,
    operation_lock_file: PathBuf,
    settings_file: PathBuf,
    managed_codex_bin: PathBuf,
}

impl Daemon {
    fn from_environment() -> Result<Self> {
        let codex_home = find_codex_home().context("failed to resolve CODEX_HOME")?;
        let socket_path = app_server_control_socket_path(codex_home.as_path())?
            .as_path()
            .to_path_buf();
        let state_dir = codex_home.as_path().join(STATE_DIR_NAME);
        let managed_codex_bin = managed_codex_bin(codex_home.as_path());
        // Old CLIs must not mistake a daemon-owned installation for their backend.
        let pid_file =
            if managed_codex_bin.starts_with(codex_home.as_path().join("packages/standalone")) {
                LEGACY_PID_FILE_NAME
            } else {
                DAEMON_PID_FILE_NAME
            };
        Ok(Self {
            log_diagnostics: false,
            socket_path,
            pid_file: state_dir.join(pid_file),
            operation_lock_file: state_dir.join(OPERATION_LOCK_FILE_NAME),
            settings_file: state_dir.join(SETTINGS_FILE_NAME),
            managed_codex_bin,
        })
    }

    fn diagnostic(&self, message: std::fmt::Arguments<'_>) {
        if self.log_diagnostics {
            tracing::info!("{message}");
        } else {
            eprintln!("{message}");
        }
    }

    fn recovery_file(&self) -> Result<PathBuf> {
        Ok(codex_app_server_transport::daemon_recovery_file_path(
            self.settings_file
                .parent()
                .and_then(Path::parent)
                .context("daemon settings path has no Codex home")?,
        ))
    }

    // Call only after taking the operation lock: an explicit update may have
    // migrated the package and PID namespace while this command was waiting.
    fn current_installation(&self) -> Result<Self> {
        let managed_codex_bin = self.current_managed_codex_bin()?;
        let home = self
            .settings_file
            .parent()
            .and_then(Path::parent)
            .context("daemon settings path has no Codex home")?;
        let pid = if managed_codex_bin.starts_with(home.join("packages/standalone")) {
            LEGACY_PID_FILE_NAME
        } else {
            DAEMON_PID_FILE_NAME
        };
        Ok(Self {
            managed_codex_bin,
            pid_file: self.pid_file.with_file_name(pid),
            ..self.clone()
        })
    }

    async fn run(&self, command: LifecycleCommand) -> Result<LifecycleOutput> {
        if command == LifecycleCommand::Version {
            return self.version().await;
        }
        let _operation_lock = self.acquire_operation_lock().await?;
        let selected = self.current_installation()?;
        match command {
            LifecycleCommand::Start => selected.start(&BTreeMap::new()).await,
            LifecycleCommand::Restart => selected.restart().await,
            LifecycleCommand::Stop => {
                let output = selected.stop().await?;
                if let Err(err) = thread_recovery::discard_pending(&selected) {
                    eprintln!("warning: failed to clear saved threads after daemon stop: {err}");
                }
                Ok(output)
            }
            LifecycleCommand::Version => unreachable!(),
        }
    }

    async fn start(&self, feature_overrides: &BTreeMap<String, bool>) -> Result<LifecycleOutput> {
        let mut managed = self.clone();
        let mut settings = self.load_settings().await?;
        let (status, backend, pid, info) = if let Ok(info) = client::probe(&self.socket_path).await
        {
            (
                LifecycleStatus::AlreadyRunning,
                self.running_backend(&settings).await?,
                None,
                info,
            )
        } else if self.running_backend_instance(&settings).await?.is_some() {
            (
                LifecycleStatus::AlreadyRunning,
                Some(BackendKind::Pid),
                None,
                self.wait_until_ready().await?,
            )
        } else {
            // A fresh start must ignore snapshots left by older stop clients.
            if let Err(err) = thread_recovery::discard_pending(self) {
                self.diagnostic(format_args!(
                    "warning: failed to clear stale daemon recovery before start: {err}"
                ));
            }
            prepare_install::prepare(self, &settings).await?;
            managed.managed_codex_bin = self.current_managed_codex_bin()?;
            managed.ensure_managed_codex_bin()?;
            // Only a fresh launch may replace these settings. Keep them for restarts
            // and updates, without changing the user's config or a running daemon.
            if settings.feature_overrides != *feature_overrides {
                settings.feature_overrides = feature_overrides.clone();
                settings.save(&self.settings_file).await?;
            }
            let pid = managed.start_managed_backend(&settings).await?;
            (
                LifecycleStatus::Started,
                Some(BackendKind::Pid),
                pid,
                self.wait_until_ready().await?,
            )
        };
        Ok(managed
            .output(status, backend, pid, Some(info.app_server_version))
            .await)
    }

    async fn restart_with_settings(&self, settings: DaemonSettings) -> Result<LifecycleOutput> {
        if client::probe(&self.socket_path).await.is_ok()
            && self.running_backend(&settings).await?.is_none()
        {
            return Err(anyhow!(
                "app server is running but is not managed by codex app-server daemon"
            ));
        }
        prepare_install::prepare(self, &settings).await?;
        let mut managed = self.clone();
        managed.managed_codex_bin = self.current_managed_codex_bin()?;

        managed.ensure_managed_codex_bin()?;
        if let Some(backend) = self.running_backend_instance(&settings).await? {
            if let Err(err) = thread_recovery::discard_pending(self) {
                eprintln!("warning: failed to clear stale daemon recovery before restart: {err}");
            }
            backend
                .stop_with_grace(settings.shutdown_grace_seconds)
                .await?;
        }

        // Persist changed launch settings only after the old process has stopped.
        // A failed or interrupted drain must not make an unapplied change look current.
        if self.load_settings().await? != settings {
            settings.save(&self.settings_file).await?;
        }
        let pid = managed.start_managed_backend(&settings).await?;
        let info = self.wait_until_ready().await?;
        Ok(managed
            .output(
                LifecycleStatus::Restarted,
                Some(BackendKind::Pid),
                pid,
                Some(info.app_server_version),
            )
            .await)
    }

    #[cfg(any(unix, windows))]
    pub(crate) async fn try_restart_if_running(
        &self,
        mode: RestartMode,
        managed_codex_bin: &Path,
    ) -> Result<RestartIfRunningOutcome> {
        let operation_lock = self.open_operation_lock_file().await?;
        if !try_lock_file(&operation_lock)? {
            return Ok(RestartIfRunningOutcome::Busy);
        }
        let settings = self.load_settings().await?;
        let outcome = if let Some(backend) = self.running_backend_instance(&settings).await? {
            let info = client::probe(&self.socket_path).await.ok();
            let managed_version = if info.is_some() {
                Some(managed_codex_version(managed_codex_bin).await?)
            } else {
                None
            };
            // The installer can retarget `current` while the updater waits for
            // this lock or probes the running server. Never restart from a
            // release that is no longer the selected latest-channel binary.
            if !self.is_stable_standalone_release()?
                || managed_install::resolved_managed_codex_bin(&self.current_managed_codex_bin()?)
                    .await
                    .ok()
                    .as_deref()
                    != Some(managed_codex_bin)
            {
                return Ok(RestartIfRunningOutcome::AlreadyCurrent);
            }
            let mode = if mode == RestartMode::IfBinaryOrVersionChanged {
                let managed_identity =
                    managed_install::executable_identity(managed_codex_bin).await?;
                if backend.running_executable_identity().await?.as_ref() == Some(&managed_identity)
                {
                    RestartMode::IfVersionChanged
                } else {
                    RestartMode::Always
                }
            } else {
                mode
            };
            match restart_decision(mode, info.as_ref(), managed_version.as_deref()) {
                RestartDecision::NotReady => {
                    diagnostics::event("daemon_not_ready", ());
                    return Ok(RestartIfRunningOutcome::NotReady);
                }
                RestartDecision::AlreadyCurrent => RestartIfRunningOutcome::AlreadyCurrent,
                RestartDecision::Restart => {
                    #[cfg(windows)]
                    backend::windows::ensure_detached_launch(managed_codex_bin)?;
                    if let Err(err) = thread_recovery::discard_pending(self) {
                        eprintln!(
                            "warning: failed to clear stale daemon recovery before update: {err}"
                        );
                    }
                    diagnostics::event(
                        "restart_requested",
                        serde_json::json!({
                                "shutdownGraceSeconds": settings.shutdown_grace_seconds,
                        }),
                    );
                    let started = std::time::Instant::now();
                    diagnostics::result(
                        "shutdown",
                        started,
                        backend
                            .stop_with_grace(settings.shutdown_grace_seconds)
                            .await,
                    )?;
                    let started = std::time::Instant::now();
                    diagnostics::result(
                        "replacement_launch",
                        started,
                        self.start_managed_backend_with_bin(&settings, managed_codex_bin)
                            .await,
                    )?;
                    let started = std::time::Instant::now();
                    diagnostics::result("readiness", started, self.wait_until_ready().await)?;
                    RestartIfRunningOutcome::Restarted
                }
            }
        } else if client::probe(&self.socket_path).await.is_ok() {
            return Err(anyhow!(
                "app server is running but is not managed by codex app-server daemon"
            ));
        } else {
            diagnostics::event("daemon_not_running", ());
            RestartIfRunningOutcome::NotRunning
        };

        if !self.is_stable_standalone_release()?
            || managed_install::resolved_managed_codex_bin(&self.current_managed_codex_bin()?)
                .await
                .ok()
                .as_deref()
                != Some(managed_codex_bin)
        {
            return Ok(RestartIfRunningOutcome::AlreadyCurrent);
        }
        Ok(outcome)
    }

    async fn stop(&self) -> Result<LifecycleOutput> {
        let settings = DaemonSettings::load_for_stop(&self.settings_file).await;
        if let Some(backend) = self.running_backend_instance(&settings).await? {
            backend
                .stop_with_grace(settings.shutdown_grace_seconds)
                .await?;
            return Ok(self
                .output(
                    LifecycleStatus::Stopped,
                    Some(BackendKind::Pid),
                    /*pid*/ None,
                    /*app_server_version*/ None,
                )
                .await);
        }

        if client::probe(&self.socket_path).await.is_ok() {
            return Err(anyhow!(
                "app server is running but is not managed by codex app-server daemon"
            ));
        }

        Ok(self
            .output(
                LifecycleStatus::NotRunning,
                /*backend*/ None,
                /*pid*/ None,
                /*app_server_version*/ None,
            )
            .await)
    }

    async fn version(&self) -> Result<LifecycleOutput> {
        let settings = self.load_settings().await?;
        let info = client::probe(&self.socket_path).await?;
        Ok(self
            .output(
                LifecycleStatus::Running,
                self.running_backend(&settings).await?,
                /*pid*/ None,
                Some(info.app_server_version),
            )
            .await)
    }

    async fn wait_until_ready(&self) -> Result<client::ProbeInfo> {
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            match client::probe(&self.socket_path).await {
                Ok(info) => return Ok(info),
                Err(err) if tokio::time::Instant::now() < deadline => {
                    let _ = err;
                    sleep(START_POLL_INTERVAL).await;
                }
                Err(err) => {
                    let context = self.app_server_not_ready_context().await;
                    return Err(err).context(context);
                }
            }
        }
    }

    async fn app_server_not_ready_context(&self) -> String {
        let mut context = format!(
            "app server did not become ready on {}",
            self.socket_path.display()
        );
        self.append_daemon_app_server_context(&mut context).await;
        backend::append_stderr_log_tail_context(&self.pid_file, &mut context).await;
        context
    }

    async fn append_daemon_app_server_context(&self, context: &mut String) {
        let managed_codex_version = self
            .managed_codex_version_best_effort()
            .await
            .unwrap_or_else(|| "unknown".to_string());
        context.push_str(&format!(
            "\n\nDaemon used app-server:\n  path: {}\n  version: {managed_codex_version}",
            self.managed_codex_bin.display()
        ));
    }

    async fn bootstrap(&self, options: BootstrapOptions) -> Result<BootstrapOutput> {
        let _operation_lock = self.acquire_operation_lock().await?;
        self.current_installation()?.bootstrap_locked(options).await
    }

    async fn bootstrap_locked(&self, _options: BootstrapOptions) -> Result<BootstrapOutput> {
        let mut settings = self.load_settings().await?;
        if client::probe(&self.socket_path).await.is_ok()
            && self.running_backend(&settings).await?.is_none()
        {
            return Err(anyhow!(
                "app server is running but is not managed by codex app-server daemon"
            ));
        }
        prepare_install::prepare(self, &settings).await?;
        let mut managed = self.clone();
        managed.managed_codex_bin = self.current_managed_codex_bin()?;
        managed.ensure_managed_codex_bin()?;
        settings.save(&self.settings_file).await?;

        if let Some(backend) = self.running_backend_instance(&settings).await? {
            if let Err(err) = thread_recovery::discard_pending(self) {
                eprintln!("warning: failed to clear stale daemon recovery before bootstrap: {err}");
            }
            backend
                .stop_with_grace(settings.shutdown_grace_seconds)
                .await?;
        }

        let backend = backend::pid_backend(managed.backend_paths(&settings));
        backend.start().await?;
        let info = self.wait_until_ready().await?;
        let managed_codex_version = managed.managed_codex_version_best_effort().await;
        Ok(BootstrapOutput {
            status: BootstrapStatus::Bootstrapped,
            backend: BackendKind::Pid,
            managed_codex_path: managed.managed_codex_bin,
            managed_codex_version,
            socket_path: self.socket_path.clone(),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            app_server_version: info.app_server_version,
        })
    }

    async fn running_backend(&self, settings: &DaemonSettings) -> Result<Option<BackendKind>> {
        Ok(self
            .running_backend_instance(settings)
            .await?
            .map(|_| BackendKind::Pid))
    }

    async fn running_backend_instance(
        &self,
        settings: &DaemonSettings,
    ) -> Result<Option<backend::PidBackend>> {
        let backend = backend::pid_backend(self.backend_paths(settings));
        if backend.is_starting_or_running().await? {
            return Ok(Some(backend));
        }
        Ok(None)
    }

    async fn start_managed_backend(&self, settings: &DaemonSettings) -> Result<Option<u32>> {
        self.start_managed_backend_with_bin(settings, &self.managed_codex_bin)
            .await
    }

    async fn start_managed_backend_with_bin(
        &self,
        settings: &DaemonSettings,
        managed_codex_bin: &Path,
    ) -> Result<Option<u32>> {
        let backend =
            backend::pid_backend(self.backend_paths_with_bin(settings, managed_codex_bin));
        backend.start().await
    }

    fn is_stable_standalone_release(&self) -> Result<bool> {
        let codex_home = self
            .settings_file
            .parent()
            .and_then(Path::parent)
            .context("daemon settings path has no Codex home")?;
        Ok(managed_install::is_stable_standalone_release(
            codex_home,
            &self.current_managed_codex_bin()?,
        ))
    }

    fn current_managed_codex_bin(&self) -> Result<PathBuf> {
        // An installer can move a legacy binary into bin/ while this updater runs.
        let home = self
            .settings_file
            .parent()
            .and_then(Path::parent)
            .context("daemon settings path has no Codex home")?;
        Ok(managed_install::managed_codex_bin(home))
    }

    async fn is_bootstrapped(&self, settings: &DaemonSettings) -> Result<bool> {
        Ok(self.running_backend_instance(settings).await?.is_some())
    }

    fn ensure_managed_codex_bin(&self) -> Result<()> {
        if self.managed_codex_bin.is_file() {
            #[cfg(windows)]
            backend::windows::ensure_detached_launch(&self.managed_codex_bin)?;
            return Ok(());
        }

        let managed_codex_path = self.managed_codex_bin.display();
        Err(anyhow!(
            "daemon executable not found at {managed_codex_path}; repair the existing installation, or run `codex app-server daemon start` to install a missing daemon"
        ))
    }

    #[cfg(any(unix, windows))]
    async fn managed_codex_version_best_effort(&self) -> Option<String> {
        managed_codex_version(&self.managed_codex_bin).await.ok()
    }

    #[cfg(not(any(unix, windows)))]
    async fn managed_codex_version_best_effort(&self) -> Option<String> {
        None
    }

    fn backend_paths(&self, settings: &DaemonSettings) -> BackendPaths {
        self.backend_paths_with_bin(settings, &self.managed_codex_bin)
    }

    fn backend_paths_with_bin(
        &self,
        settings: &DaemonSettings,
        managed_codex_bin: &Path,
    ) -> BackendPaths {
        BackendPaths {
            codex_bin: managed_codex_bin.to_path_buf(),
            pid_file: self.pid_file.clone(),
            feature_overrides: settings.feature_overrides.clone(),
        }
    }

    async fn load_settings(&self) -> Result<DaemonSettings> {
        DaemonSettings::load(&self.settings_file).await
    }

    async fn acquire_operation_lock(&self) -> Result<tokio::fs::File> {
        let operation_lock = self.open_operation_lock_file().await?;
        let deadline = tokio::time::Instant::now() + OPERATION_LOCK_TIMEOUT;
        while !try_lock_file(&operation_lock)? {
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for daemon operation lock {}",
                    self.operation_lock_file.display()
                ));
            }
            sleep(START_POLL_INTERVAL).await;
        }
        Ok(operation_lock)
    }

    async fn open_operation_lock_file(&self) -> Result<tokio::fs::File> {
        if let Some(parent) = self.operation_lock_file.parent() {
            #[cfg(unix)]
            if let Some(home) = parent.parent() {
                tokio::fs::create_dir_all(home).await?;
            }
            codex_uds::prepare_private_socket_directory(parent)
                .await
                .with_context(|| {
                    format!(
                        "failed to create daemon state directory {}",
                        parent.display()
                    )
                })?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.operation_lock_file)
            .await
            .with_context(|| {
                format!(
                    "failed to open daemon operation lock {}",
                    self.operation_lock_file.display()
                )
            })
    }

    async fn output(
        &self,
        status: LifecycleStatus,
        backend: Option<BackendKind>,
        pid: Option<u32>,
        app_server_version: Option<String>,
    ) -> LifecycleOutput {
        let managed_codex_version = self.managed_codex_version_best_effort().await;
        LifecycleOutput {
            status,
            backend,
            pid,
            managed_codex_path: self.managed_codex_bin.clone(),
            managed_codex_version,
            socket_path: self.socket_path.clone(),
            cli_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            app_server_version,
        }
    }
}

#[cfg(any(unix, windows))]
fn restart_decision(
    mode: RestartMode,
    info: Option<&client::ProbeInfo>,
    managed_version: Option<&str>,
) -> RestartDecision {
    match (mode, info, managed_version) {
        (RestartMode::IfBinaryOrVersionChanged, _, _) => {
            unreachable!("binary comparison is resolved before restart decision")
        }
        (RestartMode::IfVersionChanged, None, _) => RestartDecision::NotReady,
        (RestartMode::IfVersionChanged, Some(info), Some(managed_version))
            if info.app_server_version == managed_version =>
        {
            RestartDecision::AlreadyCurrent
        }
        _ => RestartDecision::Restart,
    }
}

#[cfg(unix)]
fn try_lock_file(file: &tokio::fs::File) -> Result<bool> {
    use std::os::fd::AsRawFd;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(err).context("failed to lock daemon operation")
}

#[cfg(not(any(unix, windows)))]
fn try_lock_file(_file: &tokio::fs::File) -> Result<bool> {
    Ok(true)
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::BackendKind;
    use super::BootstrapOutput;
    use super::BootstrapStatus;
    use super::Daemon;
    use super::LifecycleOutput;
    use super::LifecycleStatus;
    use super::RestartDecision;
    use super::RestartMode;
    use super::restart_decision;
    use crate::client::ProbeInfo;
    #[cfg(unix)]
    use crate::settings::DaemonSettings;

    #[test]
    fn restart_decision_preserves_forced_refreshes() {
        let current_info = ProbeInfo {
            app_server_version: "0.1.0".to_string(),
        };

        assert_eq!(
            [
                restart_decision(
                    RestartMode::IfVersionChanged,
                    Some(&current_info),
                    Some("0.1.0"),
                ),
                restart_decision(
                    RestartMode::IfVersionChanged,
                    /*info*/ None,
                    /*managed_version*/ None,
                ),
                restart_decision(RestartMode::Always, Some(&current_info), Some("0.1.0")),
                restart_decision(
                    RestartMode::Always,
                    /*info*/ None,
                    /*managed_version*/ None,
                ),
            ],
            [
                RestartDecision::AlreadyCurrent,
                RestartDecision::NotReady,
                RestartDecision::Restart,
                RestartDecision::Restart,
            ]
        );
    }

    #[tokio::test]
    async fn waiting_lifecycle_command_uses_migrated_installation() {
        let home = TempDir::new().expect("home");
        let state = home.path().join("app-server-daemon");
        let legacy = home.path().join("packages/standalone/current");
        std::fs::create_dir_all(&legacy).expect("legacy selection");
        let daemon = Daemon {
            log_diagnostics: false,
            socket_path: home.path().join("server.sock"),
            pid_file: state.join(super::LEGACY_PID_FILE_NAME),
            operation_lock_file: state.join("daemon.lock"),
            settings_file: state.join("settings.json"),
            managed_codex_bin: super::managed_codex_bin(home.path()),
        };
        let lock = daemon.acquire_operation_lock().await.expect("lock");
        let stop = daemon.run(super::LifecycleCommand::Stop);
        tokio::pin!(stop);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut stop)
                .await
                .is_err()
        );
        std::fs::create_dir_all(home.path().join("packages/app-server-daemon/current"))
            .expect("migrate selection");
        drop(lock);
        let output = stop.await.expect("stop");
        assert_eq!(
            (output.status, output.managed_codex_path),
            (
                LifecycleStatus::NotRunning,
                super::managed_codex_bin(home.path())
            )
        );
    }

    #[tokio::test]
    async fn stop_creates_missing_home_parent() {
        let temp = TempDir::new().expect("temp dir");
        let state = temp.path().join("missing-home").join("daemon-state");
        let daemon = Daemon {
            log_diagnostics: false,
            socket_path: state.join("server.sock"),
            pid_file: state.join("server.pid"),
            operation_lock_file: state.join("daemon.lock"),
            settings_file: state.join("settings.json"),
            managed_codex_bin: state.join("missing-codex"),
        };
        assert_eq!(
            daemon
                .run(super::LifecycleCommand::Stop)
                .await
                .expect("stop on fresh home")
                .status,
            LifecycleStatus::NotRunning,
        );
    }

    #[tokio::test]
    async fn stop_and_fresh_start_discard_pending_thread_restore() {
        let home = TempDir::new().expect("home");
        let state = home.path().join("app-server-daemon");
        codex_uds::prepare_private_socket_directory(&state)
            .await
            .expect("private state directory");
        let daemon = Daemon {
            log_diagnostics: false,
            socket_path: home.path().join("server.sock"),
            pid_file: state.join("server.pid"),
            operation_lock_file: state.join("daemon.lock"),
            settings_file: state.join("settings.json"),
            managed_codex_bin: home.path().join("codex"),
        };
        codex_app_server_transport::daemon_recovery::write_candidates(
            &daemon.recovery_file().expect("recovery path"),
            &["thread".to_string()].into_iter().collect(),
        )
        .expect("saved threads");
        assert_eq!(
            daemon
                .run(super::LifecycleCommand::Stop)
                .await
                .expect("stop")
                .status,
            LifecycleStatus::NotRunning
        );
        assert!(!daemon.recovery_file().expect("recovery path").exists());
        // Simulate an older stop client leaving a snapshot behind.
        std::fs::write(
            daemon.recovery_file().expect("recovery path"),
            r#"["thread"]"#,
        )
        .expect("legacy saved threads");
        daemon
            .run(super::LifecycleCommand::Start)
            .await
            .expect_err("missing backend binary");
        assert!(!daemon.recovery_file().expect("recovery path").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_local_backend_counts_as_bootstrapped_without_updater() {
        let home = TempDir::new().expect("home");
        let standalone = home.path().join("packages/standalone");
        let local_bin = standalone.join("local-main/bin/codex");
        tokio::fs::create_dir_all(local_bin.parent().expect("bin parent"))
            .await
            .expect("local bin directory");
        codex_utils_cargo_bin::write_executable(&local_bin, "#!/bin/sh\nexec sleep 30\n")
            .expect("local bin");
        std::os::unix::fs::symlink("local-main", standalone.join("current"))
            .expect("current local build");
        let state = home.path().join("app-server-daemon");
        let daemon = Daemon {
            log_diagnostics: false,
            socket_path: home
                .path()
                .join("app-server-control/app-server-control.sock"),
            pid_file: state.join("app-server.pid"),
            operation_lock_file: state.join("daemon.lock"),
            settings_file: state.join("settings.json"),
            managed_codex_bin: standalone.join("current/bin/codex"),
        };
        let settings = DaemonSettings::default();
        assert!(
            !daemon
                .is_bootstrapped(&settings)
                .await
                .expect("not running")
        );
        let backend = crate::backend::pid_backend(daemon.backend_paths(&settings));
        backend.start().await.expect("start local backend");
        let bootstrapped = daemon.is_bootstrapped(&settings).await;
        backend.stop().await.expect("stop local backend");
        assert!(bootstrapped.expect("managed local backend"));
    }

    #[tokio::test]
    async fn not_ready_context_reports_daemon_app_server_before_stderr() {
        let temp_dir = TempDir::new().expect("temp dir");
        let daemon = Daemon {
            log_diagnostics: false,
            socket_path: temp_dir.path().join("app-server-control.sock"),
            pid_file: temp_dir.path().join("app-server.pid"),
            operation_lock_file: temp_dir.path().join("daemon.lock"),
            settings_file: temp_dir.path().join("settings.json"),
            managed_codex_bin: temp_dir.path().join("missing-codex"),
        };
        let stderr_log = daemon.pid_file.with_extension("stderr.log");
        tokio::fs::write(&stderr_log, "unexpected argument")
            .await
            .expect("write stderr log");

        assert_eq!(
            daemon.app_server_not_ready_context().await,
            format!(
                "app server did not become ready on {}\n\n\
                 Daemon used app-server:\n  path: {}\n  version: unknown\n\n\
                 Managed app-server stderr ({}):\n  unexpected argument",
                daemon.socket_path.display(),
                daemon.managed_codex_bin.display(),
                stderr_log.display()
            )
        );
    }
}
