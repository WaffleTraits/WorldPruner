//! Running a prune in the background, for the interactive front ends.

use crate::summarize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use worldpruner::pruner::{Progress, PruneOptions, prune};
use worldpruner::world::{Dimension, LockResult, lock_world};

/// Everything needed to prune a world.
#[derive(Debug)]
pub(crate) struct Job {
    /// The folder the user chose; its `session.lock` is held during the run.
    pub world: PathBuf,
    pub dimensions: Vec<Dimension>,
    pub options: PruneOptions,
    /// Prune even if the world is locked by another program.
    pub force: bool,
}

/// Progress of a job, readable from the UI thread.
pub(crate) struct SharedProgress {
    message: Mutex<String>,
    total: AtomicU64,
    done: AtomicU64,
    /// Wakes the UI up when something changed.
    notify: Box<dyn Fn() + Send + Sync>,
}

impl SharedProgress {
    /// The current stage, the finished steps and the total number of steps.
    pub fn snapshot(&self) -> (String, u64, u64) {
        let message = self.message.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let total = self.total.load(Ordering::Relaxed);
        (message, self.done.load(Ordering::Relaxed).min(total), total)
    }
}

impl Progress for SharedProgress {
    fn begin(&self, message: &str, total: u64) {
        *self.message.lock().unwrap_or_else(|e| e.into_inner()) = message.to_string();
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        (self.notify)();
    }

    fn advance(&self, steps: u64) {
        self.done.fetch_add(steps, Ordering::Relaxed);
        (self.notify)();
    }

    fn finish(&self) {
        (self.notify)();
    }
}

/// A job running on a worker thread.
pub(crate) struct RunningJob {
    pub progress: Arc<SharedProgress>,
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub started: Instant,
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub dry_run: bool,
    cancel: Arc<AtomicBool>,
    result: Receiver<Result<String, String>>,
}

impl RunningJob {
    /// Start `job` on a new thread. `notify` is called from that thread
    /// whenever there is something new to show.
    pub fn start(job: Job, notify: impl Fn() + Send + Sync + 'static) -> RunningJob {
        let progress = Arc::new(SharedProgress {
            message: Mutex::new("Starting".to_string()),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            notify: Box::new(notify),
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, result) = channel();
        let dry_run = job.options.dry_run;
        let (worker_progress, worker_cancel) = (Arc::clone(&progress), Arc::clone(&cancel));
        std::thread::spawn(move || {
            let outcome = run_job(&job, &worker_progress, &worker_cancel);
            let _ = sender.send(outcome);
            (worker_progress.notify)();
        });
        RunningJob { progress, started: Instant::now(), dry_run, cancel, result }
    }

    /// Ask the job to stop after the files that are being processed.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelling(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The summary (or error message) of the job once it has finished.
    pub fn poll(&self) -> Option<Result<String, String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("the worker thread stopped unexpectedly".to_string())),
        }
    }
}

fn run_job(job: &Job, progress: &SharedProgress, cancel: &AtomicBool) -> Result<String, String> {
    let _lock = match lock_world(&job.world) {
        LockResult::Acquired(lock) => Some(lock),
        LockResult::InUse(path) if job.options.dry_run || job.force => {
            log::warn!("the world is in use by another program ({} is locked); continuing anyway", path.display());
            None
        }
        LockResult::InUse(path) => {
            return Err(format!(
                "The world is open in the game or on a running server ({} is locked).\nClose it first; pruning a world that is in use corrupts it.",
                path.display()
            ));
        }
        LockResult::Unknown(path, e) => {
            log::warn!("could not check whether the world is in use ({}: {e})", path.display());
            None
        }
    };
    let mut text = String::new();
    for dimension in &job.dimensions {
        let start = Instant::now();
        let report = prune(dimension, &job.options, progress, cancel).map_err(|e| format!("{e:#}"))?;
        text.push_str(&summarize(dimension, &report, start.elapsed()));
        if report.cancelled {
            break;
        }
    }
    Ok(text)
}
