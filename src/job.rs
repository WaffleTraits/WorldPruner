//! Running a prune in the background, for the interactive front ends.

use crate::summarize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
    /// Name of the current stage and when it began.
    stage: Mutex<(String, Instant)>,
    total: AtomicU64,
    done: AtomicU64,
    /// Wakes the UI up when something changed.
    notify: Box<dyn Fn() + Send + Sync>,
}

impl SharedProgress {
    /// The current stage, the finished steps and the total number of steps.
    pub fn snapshot(&self) -> (String, u64, u64) {
        let message = self.stage.lock().unwrap_or_else(|e| e.into_inner()).0.clone();
        let total = self.total.load(Ordering::Relaxed);
        (message, self.done.load(Ordering::Relaxed).min(total), total)
    }

    /// Estimated time until the current stage is finished, once there is
    /// enough progress to tell.
    pub fn eta(&self) -> Option<Duration> {
        let started = self.stage.lock().unwrap_or_else(|e| e.into_inner()).1;
        let (_, done, total) = self.snapshot();
        estimate_remaining(started.elapsed(), done, total)
    }
}

/// Time left for `total` steps if `done` of them took `elapsed`. `None`
/// while nothing is done yet (there is no rate to extrapolate from).
pub(crate) fn estimate_remaining(elapsed: Duration, done: u64, total: u64) -> Option<Duration> {
    if done == 0 || total == 0 {
        return None;
    }
    let remaining = total.saturating_sub(done);
    Some(Duration::from_secs_f64(elapsed.as_secs_f64() * remaining as f64 / done as f64))
}

/// `m:ss`, or `h:mm:ss` from an hour on.
pub(crate) fn format_clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 { format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60) } else { format!("{}:{:02}", seconds / 60, seconds % 60) }
}

impl Progress for SharedProgress {
    fn begin(&self, message: &str, total: u64) {
        *self.stage.lock().unwrap_or_else(|e| e.into_inner()) = (message.to_string(), Instant::now());
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
            stage: Mutex::new(("Starting".to_string(), Instant::now())),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_time_is_extrapolated_from_the_rate_so_far() {
        let secs = Duration::from_secs;
        assert_eq!(estimate_remaining(secs(10), 0, 100), None);
        assert_eq!(estimate_remaining(secs(10), 0, 0), None);
        assert_eq!(estimate_remaining(secs(10), 25, 100), Some(secs(30)));
        assert_eq!(estimate_remaining(secs(10), 50, 100), Some(secs(10)));
        assert_eq!(estimate_remaining(secs(10), 100, 100), Some(secs(0)));
        assert_eq!(estimate_remaining(secs(10), 120, 100), Some(secs(0)));
    }

    #[test]
    fn clock_format() {
        assert_eq!(format_clock(Duration::from_secs(0)), "0:00");
        assert_eq!(format_clock(Duration::from_secs(75)), "1:15");
        assert_eq!(format_clock(Duration::from_secs(3599)), "59:59");
        assert_eq!(format_clock(Duration::from_secs(3 * 3600 + 5 * 60 + 9)), "3:05:09");
    }

    #[test]
    fn eta_restarts_with_every_stage() {
        let progress = SharedProgress {
            stage: Mutex::new(("Starting".to_string(), Instant::now())),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            notify: Box::new(|| {}),
        };
        assert_eq!(progress.eta(), None);
        progress.begin("Scanning chunks", 4);
        assert_eq!(progress.eta(), None, "no estimate before the first step is done");
        std::thread::sleep(Duration::from_millis(20));
        progress.advance(1);
        let eta = progress.eta().unwrap();
        assert!(eta >= Duration::from_millis(60) && eta < Duration::from_secs(5), "{eta:?}");
        progress.advance(3);
        assert!(progress.eta().unwrap() < Duration::from_millis(1));
        progress.begin("Pruning files", 10);
        assert_eq!(progress.snapshot(), ("Pruning files".to_string(), 0, 10));
        assert_eq!(progress.eta(), None);
    }
}
