//! Running per-session work concurrently, and telling the user how it is going.
//!
//! Batch `analyze` and `doctor` are CPU-bound (SHA-256, zstd, JSON parsing), so they scale with
//! cores. Two properties matter more than the speed-up: the output order must not depend on
//! which thread finished first, and progress must never contaminate stdout, which is a JSON
//! document that scripts parse.

use serde::Serialize;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

/// Default worker count: bounded because the work also touches the disk, and oversubscribing a
/// single volume buys nothing.
pub fn default_jobs() -> usize {
    thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(1)
}

/// Apply `f` to every item, in parallel, returning results in the original order.
///
/// The index each worker claims is what puts the result back in place, so a run with `--jobs 8`
/// produces byte-identical output to `--jobs 1`.
pub fn map_ordered<I, O, F>(items: &[I], jobs: usize, f: F) -> Vec<O>
where
    I: Sync,
    O: Send,
    F: Fn(usize, &I) -> O + Sync,
{
    let total = items.len();
    if total == 0 {
        return Vec::new();
    }
    let workers = jobs.clamp(1, total);
    if workers == 1 {
        return items.iter().enumerate().map(|(i, it)| f(i, it)).collect();
    }

    let next = AtomicUsize::new(0);
    let collected: Mutex<Vec<(usize, O)>> = Mutex::new(Vec::with_capacity(total));
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= total {
                    break;
                }
                let out = f(index, &items[index]);
                collected
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((index, out));
            });
        }
    });

    let mut collected = collected.into_inner().unwrap_or_else(|e| e.into_inner());
    collected.sort_by_key(|(index, _)| *index);
    collected.into_iter().map(|(_, out)| out).collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProgressMode {
    /// Report only when a human is watching, i.e. stderr is a terminal.
    #[default]
    Auto,
    Always,
    Never,
}

impl ProgressMode {
    pub fn from_flags(progress: bool, no_progress: bool) -> Self {
        match (progress, no_progress) {
            (true, false) => ProgressMode::Always,
            (false, true) => ProgressMode::Never,
            _ => ProgressMode::Auto,
        }
    }
}

#[derive(Serialize)]
struct ProgressLine<'a> {
    progress: ProgressBody<'a>,
}

#[derive(Serialize)]
struct ProgressBody<'a> {
    operation: &'a str,
    done: usize,
    total: usize,
    session: &'a str,
}

/// Emits one JSON line per completed item on **stderr**.
///
/// stdout stays a single JSON document; a consumer that also reads stderr can tell these apart
/// from the final error document by the `progress` key.
pub struct Progress {
    operation: &'static str,
    total: usize,
    done: AtomicUsize,
    enabled: bool,
}

impl Progress {
    pub fn new(operation: &'static str, total: usize, mode: ProgressMode) -> Self {
        let enabled = match mode {
            ProgressMode::Always => true,
            ProgressMode::Never => false,
            ProgressMode::Auto => std::io::stderr().is_terminal(),
        } && total > 1;
        Progress {
            operation,
            total,
            done: AtomicUsize::new(0),
            enabled,
        }
    }

    pub fn item_done(&self, session: &str) {
        if !self.enabled {
            return;
        }
        let done = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        let line = ProgressLine {
            progress: ProgressBody {
                operation: self.operation,
                done,
                total: self.total,
                session,
            },
        };
        if let Ok(text) = serde_json::to_string(&line) {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{text}");
        }
    }
}
