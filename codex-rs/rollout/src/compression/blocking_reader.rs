//! Runs a rollout scan on one blocking worker with cancellation support.
use super::BlockingLineReader;
use std::io;

pub(super) async fn scan_lines<T, F>(lines: BlockingLineReader, scan: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut dyn Iterator<Item = io::Result<String>>) -> io::Result<T> + Send + 'static,
{
    let (stop, keep_running) = tokio::sync::oneshot::channel::<()>();
    let span = tracing::Span::current();
    let task = tokio::task::spawn_blocking(move || {
        span.in_scope(|| scan(&mut CancellableLines { lines, stop }))
    });
    let result = task.await.map_err(io::Error::other);
    drop(keep_running);
    result?
}

struct CancellableLines {
    lines: BlockingLineReader,
    stop: tokio::sync::oneshot::Sender<()>,
}

impl Iterator for CancellableLines {
    type Item = io::Result<String>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.stop.is_closed() {
            return None;
        }
        let line = self.lines.next();
        if self.stop.is_closed() {
            return None;
        }
        line
    }
}

#[cfg(test)]
#[path = "blocking_reader_tests.rs"]
mod tests;
