//! Durability modes and the periodic ("lazy") flusher. See
//! `Database::set_durability`.

use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Shortest accepted `Durability::Periodic` interval.
pub const MIN_FLUSH_INTERVAL: Duration = Duration::from_millis(1);
/// Longest accepted `Durability::Periodic` interval.
pub const MAX_FLUSH_INTERVAL: Duration = Duration::from_secs(60);
/// Interval of the default durability, `Durability::Periodic` (since v0.5.1).
pub const DEFAULT_FLUSH_INTERVAL: Duration = MAX_FLUSH_INTERVAL;

/// When a commit is forced to stable storage.
///
/// `Periodic` (the default since v0.5.1, every `DEFAULT_FLUSH_INTERVAL`)
/// writes each commit to the file (the OS page cache) before acknowledging
/// it but syncs from a background flusher at most once per `interval`, so a
/// process crash loses nothing, while an OS crash or power loss may lose a
/// suffix of recently acknowledged commits (up to about one interval).
/// `Full` syncs every commit before acknowledging it.
/// `None` keeps the database in memory only (`Database::create_in_memory`,
/// `Database::open_from_bytes`): nothing is written to disk and every commit
/// is lost when the process exits unless the caller saves `to_bytes()`. It
/// is the only mode an in-memory handle accepts, and file-backed handles
/// reject it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    Full,
    Periodic { interval: Duration },
    None,
}

impl Default for Durability {
    /// `Periodic` every `DEFAULT_FLUSH_INTERVAL` (60 s).
    fn default() -> Self {
        Self::Periodic {
            interval: DEFAULT_FLUSH_INTERVAL,
        }
    }
}

impl Durability {
    /// Validated `Periodic` mode (`MIN_FLUSH_INTERVAL..=MAX_FLUSH_INTERVAL`).
    pub fn periodic(interval: Duration) -> crate::Result<Self> {
        let durability = Self::Periodic { interval };
        durability.validate()?;
        Ok(durability)
    }

    /// `Periodic` with a millisecond interval, as used by CLI/JSON surfaces.
    pub fn periodic_ms(interval_ms: u64) -> crate::Result<Self> {
        Self::periodic(Duration::from_millis(interval_ms))
    }

    pub fn validate(self) -> crate::Result<()> {
        match self {
            Self::Full | Self::None => Ok(()),
            Self::Periodic { interval }
                if (MIN_FLUSH_INTERVAL..=MAX_FLUSH_INTERVAL).contains(&interval) =>
            {
                Ok(())
            }
            Self::Periodic { .. } => Err(crate::Error::InvalidDurability(
                "flush interval must be between 1 ms and 60 s",
            )),
        }
    }
}

struct Shared {
    /// Commits written since the flusher started (monotonic).
    written: AtomicU64,
    /// Highest `written` value covered by a completed sync.
    synced: AtomicU64,
    failed: AtomicBool,
    failure: Mutex<Option<(io::ErrorKind, String)>>,
    stop: Mutex<bool>,
    wake: Condvar,
    #[cfg(test)]
    flusher_syncs: AtomicU64,
    #[cfg(test)]
    fail_syncs: AtomicBool,
}

impl Shared {
    fn sync(&self, file: &File) -> io::Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(self.failure());
        }
        // Writes counted before this load completed before the sync starts.
        let target = self.written.load(Ordering::Acquire);
        if self.synced.load(Ordering::Acquire) >= target {
            return Ok(());
        }
        #[cfg(test)]
        let result = if self.fail_syncs.load(Ordering::Acquire) {
            Err(io::Error::other("injected periodic sync failure"))
        } else {
            file.sync_all()
        };
        #[cfg(not(test))]
        let result = file.sync_all();
        match result {
            Ok(()) => {
                self.synced.fetch_max(target, Ordering::AcqRel);
                Ok(())
            }
            Err(error) => {
                let mut failure = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
                failure.get_or_insert_with(|| (error.kind(), error.to_string()));
                self.failed.store(true, Ordering::Release);
                Err(error)
            }
        }
    }

    fn failure(&self) -> io::Error {
        match &*self.failure.lock().unwrap_or_else(PoisonError::into_inner) {
            Some((kind, message)) => io::Error::new(*kind, message.clone()),
            None => io::Error::other("periodic sync failed"),
        }
    }
}

/// Background flusher owning a duplicate of the datafile handle.
pub(crate) struct Flusher {
    shared: Arc<Shared>,
    file: Arc<File>,
    thread: Option<JoinHandle<()>>,
}

impl Flusher {
    pub(crate) fn start(file: File, interval: Duration) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            written: AtomicU64::new(0),
            synced: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            failure: Mutex::new(None),
            stop: Mutex::new(false),
            wake: Condvar::new(),
            #[cfg(test)]
            flusher_syncs: AtomicU64::new(0),
            #[cfg(test)]
            fail_syncs: AtomicBool::new(false),
        });
        let file = Arc::new(file);
        let thread = std::thread::Builder::new()
            .name("vlite-flusher".into())
            .spawn({
                let shared = shared.clone();
                let file = file.clone();
                move || {
                    let mut stop = shared.stop.lock().unwrap_or_else(PoisonError::into_inner);
                    loop {
                        if *stop {
                            return;
                        }
                        stop = shared
                            .wake
                            .wait_timeout(stop, interval)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0;
                        if *stop {
                            return;
                        }
                        drop(stop);
                        let pending = shared.written.load(Ordering::Acquire)
                            > shared.synced.load(Ordering::Acquire);
                        if pending {
                            #[cfg(test)]
                            shared.flusher_syncs.fetch_add(1, Ordering::Relaxed);
                            if shared.sync(&file).is_err() {
                                return;
                            }
                        }
                        stop = shared.stop.lock().unwrap_or_else(PoisonError::into_inner);
                    }
                }
            })?;
        Ok(Self {
            shared,
            file,
            thread: Some(thread),
        })
    }

    /// Records one acknowledged-but-unsynced commit (after its write).
    pub(crate) fn mark_written(&self) {
        self.shared.written.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }

    /// Commits acknowledged since the last completed sync.
    pub(crate) fn pending(&self) -> u64 {
        self.shared
            .written
            .load(Ordering::Acquire)
            .saturating_sub(self.shared.synced.load(Ordering::Acquire))
    }

    /// Forces every write made so far to stable storage.
    pub(crate) fn sync_now(&self) -> io::Result<()> {
        self.shared.sync(&self.file)
    }

    /// Stops the thread, then performs the final sync.
    pub(crate) fn shutdown(mut self) -> io::Result<()> {
        self.stop_thread();
        self.sync_now()
    }

    fn stop_thread(&mut self) {
        *self
            .shared
            .stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn flusher_syncs(&self) -> u64 {
        self.shared.flusher_syncs.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn inject_sync_failure(&self) {
        self.shared.fail_syncs.store(true, Ordering::Release);
    }
}

impl Drop for Flusher {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop_thread();
            // Destructors cannot report errors; `close` and `sync` do.
            let _ = self.sync_now();
        }
    }
}
