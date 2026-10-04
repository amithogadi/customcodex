use std::ffi::OsStr;
use std::fs::File;
use std::fs::Permissions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod blocking_reader;
mod path_metadata;
pub(crate) use path_metadata::existing_rollout_with_metadata_sync;

const COMPRESSED_SUFFIX: &str = ".zst";
const MAX_NOT_FOUND_RETRIES: usize = 3;
const OPEN_ROLLOUT_LINE_READER_RETRY_DELAY: Duration = Duration::from_millis(50);
const TEMP_SUFFIX: &str = ".tmp";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The entry point that requested a compression pass, not a Statsig cohort.
#[derive(Clone, Copy, Debug)]
pub enum RolloutCompressionTrigger {
    Startup,
    Rpc,
}

impl RolloutCompressionTrigger {
    fn tag(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Rpc => "rpc",
        }
    }
}

/// Starts a best-effort background job that compresses cold local rollout files.
///
/// The worker is fire-and-forget: failures are logged, startup is not blocked,
/// and a run marker under `codex_home` prevents overlapping or too-frequent
/// compression runs from the same local store.
pub fn spawn_rollout_compression_worker(codex_home: PathBuf, trigger: RolloutCompressionTrigger) {
    worker::spawn(codex_home, trigger)
}

/// Returns the modified time for the existing plain or compressed rollout file.
pub(crate) async fn file_modified_time(path: &Path) -> io::Result<Option<time::OffsetDateTime>> {
    Ok(path::existing_rollout_with_metadata(path)
        .await
        .and_then(|(_, metadata)| metadata.modified().ok())
        .map(time::OffsetDateTime::from))
}

/// Opens a rollout line reader that transparently handles plain `.jsonl` and `.jsonl.zst` files.
///
/// If the requested path disappears during a representation transition, this briefly retries
/// resolution so callers do not need to know which representation is on disk.
pub async fn open_rollout_line_reader(path: &Path) -> io::Result<RolloutLineReader> {
    for _ in 0..MAX_NOT_FOUND_RETRIES {
        match reader::open_once(path).await {
            Ok(inner) => return Ok(RolloutLineReader { inner }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tokio::time::sleep(OPEN_ROLLOUT_LINE_READER_RETRY_DELAY).await;
            }
            Err(err) => return Err(err),
        }
    }
    reader::open_once(path)
        .await
        .map(|inner| RolloutLineReader { inner })
}

/// Returns the compressed `.jsonl.zst` path for a rollout path.
#[cfg(test)]
pub(crate) fn compressed_rollout_path(path: &Path) -> PathBuf {
    path::compressed_rollout_path(path)
}

/// Materializes a compressed rollout back to plain `.jsonl` for async append paths.
pub(crate) async fn materialize_rollout_for_append(
    path: &Path,
    writer_lock: Option<std::sync::Arc<crate::WriterLockGuard>>,
) -> io::Result<PathBuf> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _writer_lock = writer_lock;
        materialize_rollout_for_append_blocking(path.as_path())
    })
    .await
    .map_err(io::Error::other)?
}

/// Materializes a compressed rollout back to plain `.jsonl` for blocking append paths.
pub(crate) fn materialize_rollout_for_append_blocking(path: &Path) -> io::Result<PathBuf> {
    let plain_path = plain_rollout_path(path);
    if plain_path.exists() {
        return Ok(plain_path);
    }
    let compressed_path = path::compressed_rollout_path(plain_path.as_path());
    if !compressed_path.exists() {
        return Ok(plain_path);
    }

    let temp_path = temp_path_for(plain_path.as_path(), "decompress");
    let result: io::Result<()> = (|| {
        if let Some(parent) = plain_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let metadata = std::fs::metadata(compressed_path.as_path())?;
        let permissions = metadata.permissions();
        let mut output = create_file_with_permissions(temp_path.as_path(), &permissions)?;
        {
            let input = File::open(compressed_path.as_path())?;
            let mut decoder = zstd::stream::read::Decoder::new(input)?;
            io::copy(&mut decoder, &mut output)?;
        }
        output.flush()?;
        output.sync_all()?;
        match std::fs::hard_link(temp_path.as_path(), plain_path.as_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => persist_temp_file_noclobber(temp_path.as_path(), plain_path.as_path())?,
        }
        output.set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
        output.sync_all()?;
        drop(output);
        let _ = std::fs::remove_file(temp_path.as_path());
        match std::fs::remove_file(compressed_path.as_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        Ok(())
    })();
    if let Err(_err) = &result {
        let _ = std::fs::remove_file(temp_path.as_path());
    }
    result?;
    Ok(plain_path)
}

fn persist_temp_file_noclobber(temp_path: &Path, destination: &Path) -> io::Result<()> {
    let temp_path = tempfile::TempPath::try_from_path(temp_path)?;
    match temp_path.persist_noclobber(destination) {
        Ok(()) => Ok(()),
        Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.error),
    }
}

/// Returns the plain `.jsonl` path for a plain or compressed rollout path.
pub fn plain_rollout_path(path: &Path) -> PathBuf {
    path::plain_rollout_path(path)
}

/// Parses a rollout file name, returning its plain `.jsonl` name when valid.
pub(crate) fn parse_rollout_file_name(name: &str) -> Option<&str> {
    file_name::parse_rollout_file_name(name)
}

/// A discovered rollout file, represented by exactly one physical path.
///
/// This keeps directory walkers from reimplementing the plain/compressed
/// precedence rules. The physical path may point at either `.jsonl` or
/// `.jsonl.zst`, while `plain_file_name` is always the canonical `.jsonl`
/// filename used for timestamp and id parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RolloutFile {
    path: PathBuf,
    plain_file_name: String,
}

impl RolloutFile {
    /// Creates a logical rollout file from a physical path found during discovery.
    ///
    /// Returns `None` for non-rollout names and for compressed siblings hidden by
    /// an existing plain `.jsonl` file.
    pub(crate) fn from_path(path: PathBuf) -> Option<Self> {
        let file_name = path.file_name().and_then(|name| name.to_str())?;
        let plain_file_name = file_name::parse_rollout_file_name(file_name)?.to_string();
        if path::should_skip_compressed_sibling(path.as_path()) {
            return None;
        }

        Some(Self {
            path,
            plain_file_name,
        })
    }

    /// Returns the physical path that should be opened for reads.
    pub(crate) fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Returns the canonical `.jsonl` filename for timestamp and id parsing.
    pub(crate) fn plain_file_name(&self) -> &str {
        self.plain_file_name.as_str()
    }

    /// Returns whether the physical path is the compressed representation.
    pub(crate) fn is_compressed(&self) -> bool {
        path::is_compressed_rollout_path(self.path.as_path())
    }

    /// Consumes the entry and returns the physical path that should be read.
    pub(crate) fn into_path(self) -> PathBuf {
        self.path
    }
}

/// Line-oriented rollout reader returned by [`open_rollout_line_reader`].
pub struct RolloutLineReader {
    inner: RolloutLineReaderInner,
}

enum RolloutLineReaderInner {
    Plain(tokio::io::Lines<tokio::io::BufReader<tokio::fs::File>>),
    Blocking(Option<BlockingLineReader>),
}

impl RolloutLineReader {
    /// Reads the next JSONL record from the rollout.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        match &mut self.inner {
            RolloutLineReaderInner::Plain(lines) => lines.next_line().await,
            RolloutLineReaderInner::Blocking(slot) => {
                let Some(mut reader) = slot.take() else {
                    return Err(io::Error::other("compressed rollout reader is busy"));
                };
                let (line, reader) =
                    tokio::task::spawn_blocking(move || (reader.next().transpose(), reader))
                        .await
                        .map_err(io::Error::other)?;
                *slot = Some(reader);
                line
            }
        }
    }

    /// Keeps a compressed scan on one worker with cancellation support.
    pub(crate) async fn find_map<T: Send + 'static>(
        mut self,
        mut find: impl FnMut(&str) -> Option<T> + Send + 'static,
    ) -> io::Result<Option<T>> {
        let RolloutLineReaderInner::Blocking(Some(reader)) = self.inner else {
            while let Some(line) = self.next_line().await? {
                if let Some(found) = find(&line) {
                    return Ok(Some(found));
                }
            }
            return Ok(None);
        };
        blocking_reader::scan_lines(reader, move |lines| {
            for line in lines {
                if let Some(found) = find(&line?) {
                    return Ok(Some(found));
                }
            }
            Ok(None)
        })
        .await
    }
}

type BlockingLineReader = std::io::Lines<std::io::BufReader<Box<dyn Read + Send>>>;

mod worker {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::fs::FileTimes;
    use std::fs::Permissions;
    use std::io;
    use std::io::Write;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;
    use std::time::SystemTime;

    use tracing::debug;
    use tracing::info;
    use tracing::warn;

    use tokio::task::JoinSet;

    use crate::ARCHIVED_SESSIONS_SUBDIR;
    use crate::SESSIONS_SUBDIR;

    use super::RolloutCompressionTrigger;
    use super::RolloutFile;
    use super::path;

    const TEMP_SUFFIX: &str = ".tmp";
    const COMPRESSION_LEVEL: i32 = 3;
    const MIN_ROLLOUT_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    const RUN_MARKER_STALE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
    const TEMP_FILE_STALE_AFTER: Duration = RUN_MARKER_STALE_AFTER;
    const WORKER_MAX_RUNTIME: Duration = Duration::from_secs(5 * 60 * 60);
    const RUN_MARKER_FILE_NAME: &str = "rollout-compression.lock";
    const MAX_CONCURRENT_COMPRESSION_JOBS: usize = 2;
    const MAX_METADATA_WARNINGS_PER_RUN: usize = 5;

    #[derive(Default)]
    struct CompressionStats {
        scanned: usize,
        compressed: usize,
        skipped: usize,
        failed: usize,
        scan_errors: bool,
        metadata_read_failures: usize,
        cleanup_errors: bool,
        time_budget_exhausted: bool,
    }

    pub(super) struct CompressionRunMarker {
        path: PathBuf,
        remove_on_drop: bool,
    }

    impl CompressionRunMarker {
        pub(super) fn try_claim(codex_home: &Path) -> io::Result<Option<Self>> {
            let marker_dir = codex_home.join(".tmp");
            std::fs::create_dir_all(marker_dir.as_path())?;
            let path = marker_dir.join(RUN_MARKER_FILE_NAME);
            match create_run_marker_file(path.as_path()) {
                Ok(()) => return Ok(Some(Self::new(path))),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }

            let stale = std::fs::metadata(path.as_path())
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age >= RUN_MARKER_STALE_AFTER);
            if !stale {
                return Ok(None);
            }
            match std::fs::remove_file(path.as_path()) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            match create_run_marker_file(path.as_path()) {
                Ok(()) => Ok(Some(Self::new(path))),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(None),
                Err(err) => Err(err),
            }
        }

        fn new(path: PathBuf) -> Self {
            Self {
                path,
                remove_on_drop: true,
            }
        }

        pub(super) fn persist(mut self) {
            self.remove_on_drop = false;
        }
    }

    impl Drop for CompressionRunMarker {
        fn drop(&mut self) {
            if self.remove_on_drop {
                let _ = std::fs::remove_file(self.path.as_path());
            }
        }
    }

    pub(super) fn spawn(codex_home: PathBuf, trigger: RolloutCompressionTrigger) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!(
                "failed to start rollout compression worker for {}: no Tokio runtime",
                codex_home.display()
            );
            return;
        };
        handle.spawn(async move {
            if let Err(err) = run(codex_home.clone(), trigger).await {
                warn!(
                    "rollout compression worker failed for {}: {err}",
                    codex_home.display()
                );
            }
        });
    }

    pub(super) async fn run(
        codex_home: PathBuf,
        trigger: RolloutCompressionTrigger,
    ) -> io::Result<()> {
        let Some(_maintenance_guard) =
            crate::try_acquire_rollout_maintenance_lock(codex_home.as_path())?
        else {
            debug!(
                "rollout maintenance is already running for {}",
                codex_home.display()
            );
            return Ok(());
        };
        let marker = match CompressionRunMarker::try_claim(codex_home.as_path()) {
            Ok(Some(marker)) => marker,
            Ok(None) => {
                debug!(
                    "rollout compression worker recently ran or is already running for {}",
                    codex_home.display()
                );
                return Ok(());
            }
            Err(err) => {
                return Err(err);
            }
        };

        let started_at = Instant::now();
        let writer_locks = Arc::new(crate::WriterLockCoordinator::new(&codex_home));
        let result = async {
            let mut stats = CompressionStats {
                cleanup_errors: cleanup_stale_temps(codex_home.as_path(), trigger).await?,
                ..Default::default()
            };
            for root in [
                codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
                codex_home.join(SESSIONS_SUBDIR),
            ] {
                if started_at.elapsed() >= WORKER_MAX_RUNTIME {
                    stats.time_budget_exhausted = true;
                    break;
                }
                compress_rollouts_in_root(
                    root.as_path(),
                    started_at,
                    &mut stats,
                    &writer_locks,
                    trigger,
                )
                .await?;
            }
            Ok::<_, io::Error>(stats)
        }
        .await;
        let stats = match result {
            Ok(stats) => stats,
            Err(err) => {
                return Err(err);
            }
        };
        info!(
            metadata_read_failures = stats.metadata_read_failures,
            "rollout compression worker finished: scanned={}, compressed={}, skipped={}, failed={}",
            stats.scanned,
            stats.compressed,
            stats.skipped,
            stats.failed
        );
        // Keep the existing completed outcome: it means the pass returned, not
        // that every directory was scanned or every file was compressed.
        let _completion = if stats.time_budget_exhausted {
            "time_budget"
        } else {
            "scan_finished"
        };
        marker.persist();
        Ok(())
    }

    fn create_run_marker_file(path: &Path) -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        writeln!(
            file,
            "pid={} started_at={:?}",
            std::process::id(),
            SystemTime::now()
        )?;
        Ok(())
    }

    async fn compress_rollouts_in_root(
        root: &Path,
        started_at: Instant,
        stats: &mut CompressionStats,
        writer_locks: &Arc<crate::WriterLockCoordinator>,
        trigger: RolloutCompressionTrigger,
    ) -> io::Result<()> {
        if !tokio::fs::try_exists(root).await.unwrap_or(false) {
            return Ok(());
        }
        let mut stack = vec![root.to_path_buf()];
        let mut jobs = JoinSet::new();
        while let Some(dir) = stack.pop() {
            if started_at.elapsed() >= WORKER_MAX_RUNTIME {
                stats.time_budget_exhausted = true;
                break;
            }
            let mut read_dir = match tokio::fs::read_dir(dir.as_path()).await {
                Ok(read_dir) => read_dir,
                Err(err) => {
                    stats.scan_errors = true;
                    warn!(
                        "failed to read rollout compression directory {}: {err}",
                        dir.display()
                    );
                    continue;
                }
            };
            loop {
                let entry = match read_dir.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(err) => {
                        drain_compression_jobs(&mut jobs, stats, trigger).await;
                        return Err(err);
                    }
                };
                if started_at.elapsed() >= WORKER_MAX_RUNTIME {
                    stats.time_budget_exhausted = true;
                    break;
                }
                let path = entry.path();
                let file_type = match entry.file_type().await {
                    Ok(file_type) => file_type,
                    Err(err) => {
                        stats.scan_errors = true;
                        warn!(
                            "failed to read rollout compression file type {}: {err}",
                            path.display()
                        );
                        continue;
                    }
                };
                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let Some(rollout_file) = RolloutFile::from_path(path) else {
                    continue;
                };
                if rollout_file.is_compressed() {
                    continue;
                }
                let path = rollout_file.into_path();
                let Some(rollout_id) = crate::rollout_id_from_path(path.as_path()) else {
                    stats.scan_errors = true;
                    stats.skipped = stats.skipped.saturating_add(1);
                    continue;
                };
                let thread_id = match crate::read_session_meta_line(path.as_path()).await {
                    Ok(metadata) => metadata.meta.id,
                    Err(err) => {
                        stats.scan_errors = true;
                        let reason = err
                            .get_ref()
                            .and_then(|error| {
                                error.downcast_ref::<crate::list::MetadataReadError>()
                            })
                            .map_or("io", |error| error.reason);
                        let error_kind = super::io_error_kind(&err);
                        stats.metadata_read_failures =
                            stats.metadata_read_failures.saturating_add(1);
                        if stats.metadata_read_failures <= MAX_METADATA_WARNINGS_PER_RUN {
                            let metadata = tokio::fs::metadata(&path).await.ok();
                            let file_size_bytes = metadata.as_ref().map(std::fs::Metadata::len);
                            let mtime_age_seconds = metadata
                                .and_then(|metadata| metadata.modified().ok())
                                .and_then(|modified| modified.elapsed().ok())
                                .map(|age| age.as_secs());
                            // Only bounded labels and parsed IDs, never paths or error messages.
                            warn!(
                                trigger = trigger.tag(),
                                %rollout_id,
                                reason,
                                error_kind,
                                file_size_bytes,
                                mtime_age_seconds,
                                "skipping rollout compression because session metadata could not be read"
                            );
                        }
                        stats.skipped = stats.skipped.saturating_add(1);
                        continue;
                    }
                };
                stats.scanned = stats.scanned.saturating_add(1);
                while jobs.len() >= MAX_CONCURRENT_COMPRESSION_JOBS {
                    collect_next_compression_job(&mut jobs, stats, trigger).await;
                }
                let writer_locks = Arc::clone(writer_locks);
                jobs.spawn_blocking(move || {
                    let result = compress_rollout_if_cold_blocking(
                        path.as_path(),
                        &writer_locks,
                        thread_id,
                        trigger,
                    );
                    (path, result)
                });
            }
        }
        drain_compression_jobs(&mut jobs, stats, trigger).await;
        Ok(())
    }

    type CompressionJobResult = (PathBuf, io::Result<CompressionOutcome>);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CompressionOutcome {
        Compressed,
        SkippedNotCold,
        SkippedBusy,
        SkippedChanged,
        SkippedAlreadyCompressed,
    }

    enum ColdFileState {
        Cold(FileState),
        NotCold(Option<FileState>),
    }

    async fn drain_compression_jobs(
        jobs: &mut JoinSet<CompressionJobResult>,
        stats: &mut CompressionStats,
        trigger: RolloutCompressionTrigger,
    ) {
        while !jobs.is_empty() {
            collect_next_compression_job(jobs, stats, trigger).await;
        }
    }

    async fn collect_next_compression_job(
        jobs: &mut JoinSet<CompressionJobResult>,
        stats: &mut CompressionStats,
        _trigger: RolloutCompressionTrigger,
    ) {
        let Some(result) = jobs.join_next().await else {
            return;
        };
        match result {
            Ok((_, Ok(outcome))) => match outcome {
                CompressionOutcome::Compressed => {
                    stats.compressed = stats.compressed.saturating_add(1);
                }
                CompressionOutcome::SkippedNotCold
                | CompressionOutcome::SkippedBusy
                | CompressionOutcome::SkippedChanged
                | CompressionOutcome::SkippedAlreadyCompressed => {
                    stats.skipped = stats.skipped.saturating_add(1);
                }
            },
            Ok((path, Err(err))) => {
                stats.failed = stats.failed.saturating_add(1);
                // Keep failures visible in local diagnostics.
                warn!("failed to compress rollout {}: {err}", path.display());
            }
            Err(err) => {
                stats.failed = stats.failed.saturating_add(1);
                warn!("rollout compression task failed: {err}");
            }
        }
    }

    fn compress_rollout_if_cold_blocking(
        path: &Path,
        writer_locks: &Arc<crate::WriterLockCoordinator>,
        thread_id: codex_protocol::ThreadId,
        _trigger: RolloutCompressionTrigger,
    ) -> io::Result<CompressionOutcome> {
        let before = match cold_file_state(path)? {
            ColdFileState::Cold(state) => state,
            ColdFileState::NotCold(_state) => {
                return Ok(CompressionOutcome::SkippedNotCold);
            }
        };
        let compressed_path = path::compressed_rollout_path(path);
        if compressed_path.exists() {
            return Ok(CompressionOutcome::SkippedAlreadyCompressed);
        }

        let temp_dir = compressed_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(temp_dir)?;
        let mut temp_file = tempfile::Builder::new()
            .prefix("rollout-compress-")
            .suffix(TEMP_SUFFIX)
            .tempfile_in(temp_dir)?;
        encode_zstd_to_writer(path, temp_file.as_file_mut())?;
        temp_file.as_file_mut().flush()?;
        verify_zstd(temp_file.path())?;
        if !same_file_state(path, &before)? {
            return Ok(CompressionOutcome::SkippedChanged);
        }
        set_file_metadata(temp_file.as_file(), before.modified, &before.permissions)?;
        temp_file.as_file().sync_all()?;
        // Encoding and verification do not block writers. Coordination prevents writer
        // acquisition while we recheck, publish, and remove the original file.
        let Some(_publication_guard) = writer_locks.try_acquire_for_publication(thread_id)? else {
            return Ok(CompressionOutcome::SkippedBusy);
        };
        if !same_file_state(path, &before)? {
            return Ok(CompressionOutcome::SkippedChanged);
        }

        match temp_file.persist_noclobber(compressed_path.as_path()) {
            Ok(_) => {}
            Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(CompressionOutcome::SkippedAlreadyCompressed);
            }
            Err(err) => {
                return Err(err.error);
            }
        }
        if !same_file_state(path, &before)? {
            let _ = std::fs::remove_file(compressed_path.as_path());
            return Ok(CompressionOutcome::SkippedChanged);
        }
        std::fs::remove_file(path)?;
        Ok(CompressionOutcome::Compressed)
    }

    struct FileState {
        len: u64,
        modified: SystemTime,
        permissions: Permissions,
    }

    fn cold_file_state(path: &Path) -> io::Result<ColdFileState> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(ColdFileState::NotCold(None));
            }
            Err(err) => return Err(err),
        };
        if !metadata.is_file() {
            return Ok(ColdFileState::NotCold(None));
        }
        let modified = metadata.modified()?;
        let state = FileState {
            len: metadata.len(),
            modified,
            permissions: metadata.permissions(),
        };
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        if age < MIN_ROLLOUT_AGE {
            return Ok(ColdFileState::NotCold(Some(state)));
        }
        Ok(ColdFileState::Cold(state))
    }

    fn same_file_state(path: &Path, expected: &FileState) -> io::Result<bool> {
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(metadata.len() == expected.len
                && metadata.modified()? == expected.modified
                && metadata.permissions() == expected.permissions),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn encode_zstd_to_writer(source: &Path, output: impl Write) -> io::Result<()> {
        let mut input = File::open(source)?;
        let mut encoder = zstd::stream::write::Encoder::new(output, COMPRESSION_LEVEL)?;
        // Preserve fast byte-bound checks for paginated history without decoding the whole file.
        encoder.set_pledged_src_size(Some(input.metadata()?.len()))?;
        io::copy(&mut input, &mut encoder)?;
        encoder.finish()?;
        Ok(())
    }

    fn verify_zstd(path: &Path) -> io::Result<()> {
        let input = File::open(path)?;
        let mut decoder = zstd::stream::read::Decoder::new(input)?;
        let mut sink = io::sink();
        io::copy(&mut decoder, &mut sink)?;
        Ok(())
    }

    fn set_file_metadata(
        file: &File,
        modified: SystemTime,
        permissions: &Permissions,
    ) -> io::Result<()> {
        file.set_times(FileTimes::new().set_modified(modified))?;
        file.set_permissions(permissions.clone())
    }

    async fn cleanup_stale_temps(
        codex_home: &Path,
        trigger: RolloutCompressionTrigger,
    ) -> io::Result<bool> {
        let mut errors = false;
        for root in [
            codex_home.join(SESSIONS_SUBDIR),
            codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
        ] {
            errors |= cleanup_stale_temps_in_root(root.as_path(), trigger).await?;
        }
        Ok(errors)
    }

    async fn cleanup_stale_temps_in_root(
        root: &Path,
        _trigger: RolloutCompressionTrigger,
    ) -> io::Result<bool> {
        let mut errors = false;
        if !tokio::fs::try_exists(root).await.unwrap_or(false) {
            return Ok(errors);
        }
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut read_dir = match tokio::fs::read_dir(dir.as_path()).await {
                Ok(read_dir) => read_dir,
                Err(err) => {
                    errors = true;
                    warn!(
                        "failed to read rollout temp cleanup directory {}: {err}",
                        dir.display()
                    );
                    continue;
                }
            };
            while let Some(entry) = read_dir.next_entry().await? {
                let path = entry.path();
                let file_type = match entry.file_type().await {
                    Ok(file_type) => file_type,
                    Err(err) => {
                        errors = true;
                        warn!(
                            "failed to read rollout temp cleanup file type {}: {err}",
                            path.display()
                        );
                        continue;
                    }
                };
                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                if file_type.is_file()
                    && path
                        .file_name()
                        .and_then(OsStr::to_str)
                        .is_some_and(|name| name.ends_with(TEMP_SUFFIX))
                {
                    let stale = entry
                        .metadata()
                        .await
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age >= TEMP_FILE_STALE_AFTER);
                    if !stale {
                        continue;
                    }
                    match tokio::fs::remove_file(path.as_path()).await {
                        Ok(()) => {}
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                        Err(err) => {
                            errors = true;
                            warn!(
                                "failed to remove stale rollout temp {}: {err}",
                                path.display()
                            );
                        }
                    }
                }
            }
        }
        Ok(errors)
    }
}

/// Returns the existing rollout path, preferring the plain `.jsonl` file over
/// its `.jsonl.zst` compressed sibling.
pub async fn existing_rollout_path(path: &Path) -> Option<PathBuf> {
    path::existing_rollout_path(path).await
}

mod path {
    use std::ffi::OsStr;
    use std::fs::Metadata;
    use std::path::Path;
    use std::path::PathBuf;

    use super::COMPRESSED_SUFFIX;

    pub(super) fn compressed_rollout_path(path: &Path) -> PathBuf {
        if is_compressed_rollout_path(path) {
            return path.to_path_buf();
        }
        let mut file_name = path
            .file_name()
            .map(OsStr::to_os_string)
            .unwrap_or_else(|| OsStr::new("rollout.jsonl").to_os_string());
        file_name.push(COMPRESSED_SUFFIX);
        path.with_file_name(file_name)
    }

    pub(super) fn plain_rollout_path(path: &Path) -> PathBuf {
        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            return path.to_path_buf();
        };
        let Some(plain_file_name) = file_name.strip_suffix(COMPRESSED_SUFFIX) else {
            return path.to_path_buf();
        };
        path.with_file_name(plain_file_name)
    }

    pub(super) fn is_compressed_rollout_path(path: &Path) -> bool {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with(".jsonl.zst"))
    }

    pub(super) fn should_skip_compressed_sibling(path: &Path) -> bool {
        is_compressed_rollout_path(path) && plain_rollout_path(path).exists()
    }

    pub(super) async fn existing_rollout_path(path: &Path) -> Option<PathBuf> {
        existing_rollout_with_metadata(path)
            .await
            .map(|(path, _)| path)
    }

    /// Resolves the plain rollout before its compressed sibling and retains the lookup metadata.
    ///
    /// Returning the metadata lets callers inspect the selected file without a second stat.
    pub(super) async fn existing_rollout_with_metadata(path: &Path) -> Option<(PathBuf, Metadata)> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || super::existing_rollout_with_metadata_sync(&path))
            .await
            .ok()
            .flatten()
    }
}

mod file_name {
    use super::COMPRESSED_SUFFIX;

    pub(super) fn parse_rollout_file_name(name: &str) -> Option<&str> {
        let name = name.strip_suffix(COMPRESSED_SUFFIX).unwrap_or(name);
        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
            Some(name)
        } else {
            None
        }
    }
}

mod reader {
    use super::RolloutLineReaderInner;
    use super::path;
    use std::fs::File;
    use std::io;
    use std::io::BufRead;
    use std::io::Read;
    use std::path::Path;
    use tokio::io::AsyncBufReadExt;

    pub(super) async fn open_once(path: &Path) -> io::Result<RolloutLineReaderInner> {
        let path = path::existing_rollout_path(path)
            .await
            .unwrap_or_else(|| path.to_path_buf());
        if path::is_compressed_rollout_path(path.as_path()) {
            let reader = tokio::task::spawn_blocking(move || {
                let input = File::open(path.as_path())?;
                let decoder = zstd::stream::read::Decoder::new(input)?;
                Ok::<_, io::Error>(
                    io::BufReader::new(Box::new(decoder) as Box<dyn Read + Send>).lines(),
                )
            })
            .await
            .map_err(io::Error::other)??;
            return Ok(RolloutLineReaderInner::Blocking(Some(reader)));
        }
        let file = tokio::fs::File::open(path).await?;
        Ok(RolloutLineReaderInner::Plain(
            tokio::io::BufReader::new(file).lines(),
        ))
    }
}

#[cfg(unix)]
fn create_file_with_permissions(path: &Path, permissions: &Permissions) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(permissions.mode() & 0o7777)
        .open(path)?;
    file.set_permissions(permissions.clone())?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_file_with_permissions(path: &Path, permissions: &Permissions) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.set_permissions(permissions.clone())?;
    Ok(file)
}

fn temp_path_for(path: &Path, operation: &str) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_else(|| OsStr::new("rollout").to_os_string());
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    file_name.push(format!(
        ".{operation}.{}.{counter}{TEMP_SUFFIX}",
        std::process::id()
    ));
    path.with_file_name(file_name)
}

#[cfg(test)]
#[path = "compression_tests.rs"]
mod tests;

pub(super) fn io_error_kind(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::AlreadyExists => "already_exists",
        io::ErrorKind::InvalidInput => "invalid_input",
        io::ErrorKind::InvalidData => "invalid_data",
        io::ErrorKind::TimedOut => "timed_out",
        io::ErrorKind::WriteZero => "write_zero",
        io::ErrorKind::Interrupted => "interrupted",
        io::ErrorKind::Unsupported => "unsupported",
        io::ErrorKind::UnexpectedEof => "unexpected_eof",
        io::ErrorKind::StorageFull => "storage_full",
        io::ErrorKind::ReadOnlyFilesystem => "read_only_filesystem",
        io::ErrorKind::NotADirectory => "not_a_directory",
        io::ErrorKind::IsADirectory => "is_a_directory",
        _ => "other",
    }
}
