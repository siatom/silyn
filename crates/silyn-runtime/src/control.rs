//! Cooperative run cancellation.
//!
//! Cancellation is signalled through an in-process flag and, optionally, a
//! `<home>/run/<run_id>.cancel` file that another process can create. The run
//! loop polls both while it is active so an in-flight model or tool call is
//! interrupted at the next check.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use silyn_core::ids::RunId;
use tokio::sync::watch;

/// How often a cancel file is polled while a run is active.
pub const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Path of the optional cross-process cancel marker for `run_id`.
#[must_use]
pub fn cancel_file_path(home: &Path, run_id: RunId) -> PathBuf {
    home.join("run").join(format!("{run_id}.cancel"))
}

/// Cloneable cancellation handle shared by the run loop and its callers.
#[derive(Clone)]
pub struct RunControl {
    cancelled: Arc<AtomicBool>,
    sender: Arc<watch::Sender<bool>>,
    file: Option<Arc<PathBuf>>,
}

impl Default for RunControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RunControl {
    /// Creates an in-process cancellation flag.
    #[must_use]
    pub fn new() -> Self {
        let (sender, _receiver) = watch::channel(false);
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            sender: Arc::new(sender),
            file: None,
        }
    }

    /// Creates a handle that also observes `path` for as long as the run is
    /// active. Calling [`cancel`](Self::cancel) on this handle writes the file.
    #[must_use]
    pub fn with_cancel_file(path: PathBuf) -> Self {
        let mut control = Self::new();
        control.file = Some(Arc::new(path));
        control
    }

    /// Requests cancellation. Best effort: the in-process flag is authoritative
    /// even when the cancel file cannot be written.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let _ = self.sender.send(true);
        if let Some(path) = &self.file
            && let Some(parent) = path.parent()
        {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::write(path.as_ref(), b"cancel\n");
        }
    }

    /// Returns true when cancellation was requested locally or through the file.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
            || self.file.as_ref().is_some_and(|path| path.exists())
    }

    /// Resolves once cancellation has been requested.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let mut receiver = self.sender.subscribe();
            if *receiver.borrow() {
                return;
            }
            let _ = tokio::time::timeout(CANCEL_POLL_INTERVAL, receiver.changed()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_file_path_is_scoped_to_the_run() {
        let run_id = RunId::new();
        let path = cancel_file_path(Path::new("/tmp/silyn"), run_id);
        assert!(path.ends_with(format!("{run_id}.cancel")));
        assert!(path.parent().and_then(Path::file_name).is_some());
    }

    #[tokio::test]
    async fn flag_is_visible_to_every_clone_and_wakes_waiters() {
        let control = RunControl::new();
        let clone = control.clone();
        assert!(!control.is_cancelled());

        let waiter = tokio::spawn(async move { clone.cancelled().await });
        control.cancel();

        assert!(control.is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter wakes")
            .expect("waiter joined");
    }

    #[tokio::test]
    async fn cancel_file_is_observed_by_another_handle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = cancel_file_path(dir.path(), RunId::new());
        let observer = RunControl::with_cancel_file(path.clone());
        let writer = RunControl::with_cancel_file(path.clone());

        let waiter = tokio::spawn(async move { observer.cancelled().await });
        writer.cancel();

        assert!(path.exists());
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("file watcher wakes")
            .expect("file watcher joined");
    }
}
