//! Per-connection log of every query sent to the server (shown in the Console panel).

//!
//! Drivers report through [`timed`] (or [`note`] from blocking code) at the point where SQL
//! goes out, so the console shows what the server actually received. Where the entry goes
//! and how it is classified comes from the task-local scope set by the logging wrapper
//! (`db::logged`), so drivers don't need to know about the log.

use std::collections::VecDeque;
use std::fmt::Display;
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Oldest entries are dropped beyond this.
const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// Lookups the app makes by itself: table lists, structure, keys, versions...
    Meta,
    /// Reading and writing table data from the grid (browse, count, commit, import, export).
    Data,
    /// Statements typed by the user in a query tab.
    User,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub seq: u64,
    pub at: SystemTime,
    pub kind: LogKind,
    pub sql: String,
    pub duration: Duration,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct QueryLog {
    entries: Mutex<VecDeque<LogEntry>>,
    next: AtomicU64,
    /// Driver calls running right now, and started so far (drives the loading animation).
    in_flight: AtomicUsize,
    started: AtomicU64,
}

/// Marks a driver call as running until dropped.
pub struct Busy<'a>(&'a QueryLog);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl QueryLog {
    pub fn new() -> Arc<Self> {
        Arc::new(QueryLog::default())
    }

    pub fn record<T, E: std::fmt::Display>(
        &self,
        kind: LogKind,
        sql: &str,
        duration: Duration,
        result: &Result<T, E>,
    ) {
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        let entry = LogEntry {
            seq: self.next.fetch_add(1, Ordering::Relaxed) + 1,
            at: SystemTime::now(),
            kind,
            sql: sql.trim().to_string(),
            duration,
            error,
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.push_back(entry);
        while entries.len() > MAX_ENTRIES {
            entries.pop_front();
        }
    }

    /// Entries newer than `seq`, oldest first.
    pub fn since(&self, seq: u64) -> Vec<LogEntry> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.iter().filter(|e| e.seq > seq).cloned().collect()
    }

    pub fn busy(&self) -> Busy<'_> {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        self.started.fetch_add(1, Ordering::Relaxed);
        Busy(self)
    }

    /// `(running now, started so far)`: a changed count means something ran since last asked,
    /// even if it already finished.
    pub fn activity(&self) -> (usize, u64) {
        (self.in_flight.load(Ordering::Relaxed), self.started.load(Ordering::Relaxed))
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// Where queries of the running driver call are recorded, and as what.
#[derive(Clone)]
pub struct Sink {
    pub log: Arc<QueryLog>,
    pub kind: LogKind,
}

tokio::task_local! {
    static SCOPE: Sink;
}

/// Run `fut` with its queries recorded into `sink`.
pub async fn scoped<F: Future>(sink: Sink, fut: F) -> F::Output {
    SCOPE.scope(sink, fut).await
}

/// The sink of the current task, for handing to blocking threads.
pub fn current() -> Option<Sink> {
    SCOPE.try_with(Sink::clone).ok()
}

/// Run a query future and record it with its duration and outcome.
pub async fn timed<T, E: Display>(sql: &str, fut: impl Future<Output = Result<T, E>>) -> Result<T, E> {
    let start = Instant::now();
    let result = fut.await;
    note(&current(), sql, start, &result);
    result
}

/// Record a query that started at `start` (for code that can't use [`timed`]).
pub fn note<T, E: Display>(sink: &Option<Sink>, sql: &str, start: Instant, result: &Result<T, E>) {
    if let Some(sink) = sink {
        sink.log.record(sink.kind, sql, start.elapsed(), result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_trims() {
        let log = QueryLog::new();
        log.record(LogKind::Meta, " SELECT 1 ", Duration::from_millis(2), &Ok::<(), String>(()));
        log.record(LogKind::User, "BAD", Duration::ZERO, &Err::<(), _>("syntax error"));
        let all = log.since(0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].sql, "SELECT 1");
        assert_eq!(all[1].error.as_deref(), Some("syntax error"));
        assert_eq!(log.since(all[0].seq).len(), 1);
        log.clear();
        assert!(log.since(0).is_empty());
    }

    #[tokio::test]
    async fn scoped_timed_records() {
        let log = QueryLog::new();
        // Outside a scope nothing is recorded.
        timed("SELECT 0", async { Ok::<_, String>(()) }).await.unwrap();
        let sink = Sink { log: log.clone(), kind: LogKind::Data };
        scoped(sink, async {
            timed("SELECT 1", async { Ok::<_, String>(()) }).await.unwrap();
        })
        .await;
        let all = log.since(0);
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].sql.as_str(), all[0].kind), ("SELECT 1", LogKind::Data));
    }
}
