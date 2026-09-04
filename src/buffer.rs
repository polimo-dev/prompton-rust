//! The monitoring-log buffer: batching, retries, and the rules that keep a log flush from ever
//! touching a model call.
//!
//! Records are queued by [`crate::Client::log`] and sent by a background thread on whichever of
//! the three triggers comes first — size, bytes or time. One request carries at most 200 records
//! and stays well under the server's 5 MB body limit, and every request covers exactly one
//! environment, because `environment` is forced onto the whole batch.
//!
//! What happens when a send fails is the interesting part:
//!
//! | answer | what the buffer does |
//! |---|---|
//! | `202` | reads `rejected`, never resends what was accepted |
//! | `429`, any `5xx`, transport failure | resends **the same batch with the same ids** after `Retry-After` (else 1 s doubling to 5 min), for a bounded number of attempts, then drops it and counts it |
//! | `413` | splits the batch in half and sends both halves |
//! | any other `4xx` | drops the batch, counts it, and says so once |
//!
//! While a batch is being retried it stays at the head of the queue and later records queue
//! behind it. Above `max_queue` the **oldest** records are dropped and counted, so a PromptOn
//! outage costs bounded memory, never the app's stability.
//!
//! Shutdown drains whatever can go out immediately and then stops. It neither waits out nor
//! sends into an armed `Retry-After`, and it gives up after [`SHUTDOWN_DRAIN`], so a process
//! exiting during a PromptOn outage is never held for the backoff ladder.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::api::{Api, LogsAck};
use crate::config::{Config, Mode, MAX_BACKOFF};
use crate::error::Error;
use crate::logger::Logger;

/// The hard cap on records per request, from the runtime API.
pub const MAX_RECORDS_PER_REQUEST: usize = 200;
/// The request size the buffer keeps batches under; the server's body limit is 5 MB.
pub const MAX_REQUEST_BYTES: usize = 4_000_000;
/// The first retry delay; it doubles up to five minutes.
pub const RETRY_BASE: Duration = Duration::from_secs(1);
/// How long the background thread keeps draining after [`Buffer::stop`] before it gives up.
///
/// Shutdown is best effort and best effort has to be *quick*: a process on its way out must not
/// be held for a retry pause the server asked for, so the drain neither waits one out nor sends
/// into it.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// What the buffer has done so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogStats {
    /// Records waiting to be sent.
    pub queued: usize,
    /// Records dropped because the queue was full.
    pub dropped_oldest: u64,
    /// Records dropped because one record alone was over the request limit.
    pub dropped_too_large: u64,
    /// Records dropped after a batch exhausted its retries or hit a 4xx.
    pub dropped_undeliverable: u64,
    /// Requests the buffer has sent, whatever the answer was — a 4xx, a 5xx and a transport
    /// failure all count, so send volume stays visible during an outage.
    pub requests: u64,
    /// Records the server stored.
    pub accepted: u64,
    /// Records the server had already stored.
    pub duplicates: u64,
    /// Records the server refused.
    pub rejected: u64,
}

/// What one [`crate::Client::flush`] achieved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlushResult {
    /// Requests this flush completed; a failing send ends the flush and comes back as the error.
    pub requests: u64,
    /// Records the server stored.
    pub accepted: u64,
    /// Records the server had already stored.
    pub duplicates: u64,
    /// Records the server refused.
    pub rejected: u64,
    /// Records still queued when the flush returned.
    pub queued: usize,
}

#[derive(Debug, Clone)]
struct Item {
    environment: String,
    record: Map<String, Value>,
    bytes: usize,
}

#[derive(Debug, Default)]
struct State {
    queue: VecDeque<Item>,
    /// Batches to send exactly as they are: a retry, or the two halves of a 413.
    pending: VecDeque<Vec<Item>>,
    bytes: usize,
    in_flight: usize,
    oldest_queued_at: Option<Instant>,
    attempts: u32,
    next_attempt: Option<Instant>,
    stop: bool,
    stopped_at: Option<Instant>,
    stats: LogStats,
    captured: Vec<Map<String, Value>>,
}

impl State {
    fn total(&self) -> usize {
        self.queue.len() + self.pending.iter().map(Vec::len).sum::<usize>()
    }
}

pub(crate) struct Buffer {
    config: Arc<Config>,
    api: Arc<Api>,
    logger: Logger,
    state: Mutex<State>,
    signal: Condvar,
}

impl Buffer {
    pub fn new(config: Arc<Config>, api: Arc<Api>, logger: Logger) -> Buffer {
        Buffer {
            config,
            api,
            logger,
            state: Mutex::new(State::default()),
            signal: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Queues one record. Returns immediately; the caller is never blocked on the network.
    pub fn enqueue(&self, record: Map<String, Value>, environment: Option<String>) {
        let environment = environment.unwrap_or_else(|| self.config.environment.clone());
        let bytes = serde_json::to_vec(&record)
            .map(|bytes| bytes.len())
            .unwrap_or(0);

        let mut state = self.lock();

        if self.config.mode != Mode::Live {
            // Offline and test mode keep records in memory instead of sending them; the same cap
            // applies, so a long-running offline process cannot grow without bound.
            while state.captured.len() >= self.config.log.max_queue {
                state.captured.remove(0);
                state.stats.dropped_oldest += 1;
            }
            state.captured.push(record);
            return;
        }

        if bytes > MAX_REQUEST_BYTES {
            state.stats.dropped_too_large += 1;
            drop(state);
            self.logger.say(format!(
                "dropping one monitoring log of {bytes} bytes: a single record cannot exceed {MAX_REQUEST_BYTES} bytes"
            ));
            return;
        }

        while state.total() >= self.config.log.max_queue {
            if state.queue.pop_front().is_some() {
                state.stats.dropped_oldest += 1;
            } else if let Some(batch) = state.pending.pop_front() {
                state.stats.dropped_oldest += batch.len() as u64;
            } else {
                break;
            }
        }

        if state.oldest_queued_at.is_none() {
            state.oldest_queued_at = Some(Instant::now());
        }
        state.bytes += bytes;
        state.queue.push_back(Item {
            environment,
            record,
            bytes,
        });

        let should_wake = state.pending.is_empty()
            && (state.queue.len() >= self.config.log.flush_size
                || state.bytes >= self.config.log.flush_bytes);
        drop(state);
        if should_wake {
            self.signal.notify_all();
        }
    }

    /// The records captured in test and offline mode.
    pub fn captured(&self) -> Vec<Map<String, Value>> {
        self.lock().captured.clone()
    }

    /// Forgets the captured records.
    pub fn clear_captured(&self) {
        self.lock().captured.clear();
    }

    /// What the buffer has done so far.
    pub fn stats(&self) -> LogStats {
        let state = self.lock();
        let mut stats = state.stats.clone();
        stats.queued = state.total();
        stats
    }

    /// Whether a failed batch has asked to be left alone until its `Retry-After` has elapsed.
    fn paused(&self) -> bool {
        let now = Instant::now();
        self.lock()
            .next_attempt
            .map(|next| next > now)
            .unwrap_or(false)
    }

    /// Sends everything queued and waits for the answers, including the batches the background
    /// thread is already sending.
    ///
    /// This is the user-called path, so it sends now even inside an armed `Retry-After` window:
    /// the caller asked explicitly. The background thread and the shutdown drain both wait.
    ///
    /// Returns the first error a send hit; the batch that failed stays queued, so a later flush
    /// (or the background thread) retries it with the same ids.
    pub fn flush(&self, deadline: Option<Instant>) -> Result<FlushResult, Error> {
        self.drain_queue(deadline, false)
    }

    /// The shutdown drain: like [`Buffer::flush`], but it leaves an armed `Retry-After` window
    /// alone instead of sending into it. Whatever is still queued is lost, which is what best
    /// effort means when the process is exiting.
    pub fn drain(&self, deadline: Option<Instant>) -> Result<FlushResult, Error> {
        self.drain_queue(deadline, true)
    }

    fn drain_queue(
        &self,
        deadline: Option<Instant>,
        respect_pause: bool,
    ) -> Result<FlushResult, Error> {
        let mut outcome = FlushResult::default();
        let mut error = None;

        loop {
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    break;
                }
            }
            if respect_pause && self.paused() {
                break;
            }
            let batch = match self.take_batch(true) {
                Some(batch) => batch,
                None => break,
            };
            match self.send(batch) {
                Ok(ack) => {
                    outcome.requests += 1;
                    outcome.accepted += ack.accepted as u64;
                    outcome.duplicates += ack.duplicates as u64;
                    outcome.rejected += ack.rejected.len() as u64;
                }
                Err(err) => {
                    error = Some(err);
                    break;
                }
            }
        }

        // Wait for whatever the background thread is sending right now, so that when flush()
        // returns the counters are final.
        let deadline = deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
        loop {
            let state = self.lock();
            if state.in_flight == 0 {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (_state, _) = match self.signal.wait_timeout(state, deadline - now) {
                Ok(result) => result,
                Err(poisoned) => poisoned.into_inner(),
            };
        }

        outcome.queued = self.lock().total();
        match error {
            Some(error) => Err(error),
            None => Ok(outcome),
        }
    }

    /// Takes the next request's worth of records: a pending batch as it is, otherwise up to 200
    /// records of one environment from the queue.
    fn take_batch(&self, ignore_thresholds: bool) -> Option<Vec<Item>> {
        let mut state = self.lock();
        if let Some(batch) = state.pending.pop_front() {
            state.in_flight += 1;
            return Some(batch);
        }
        if state.queue.is_empty() {
            state.oldest_queued_at = None;
            state.next_attempt = None;
            return None;
        }

        if !ignore_thresholds {
            let due = state
                .oldest_queued_at
                .map(|at| at.elapsed() >= self.config.log.flush_interval)
                .unwrap_or(false);
            let full = state.queue.len() >= self.config.log.flush_size
                || state.bytes >= self.config.log.flush_bytes;
            if !due && !full && !state.stop {
                return None;
            }
        }

        let environment = state.queue.front()?.environment.clone();
        let mut batch: Vec<Item> = Vec::new();
        let mut bytes = 0usize;
        while batch.len() < MAX_RECORDS_PER_REQUEST.min(self.config.log.flush_size.max(1)) {
            let next = match state.queue.front() {
                Some(next) => next,
                None => break,
            };
            if next.environment != environment {
                break;
            }
            if !batch.is_empty() && bytes + next.bytes > MAX_REQUEST_BYTES {
                break;
            }
            let item = state.queue.pop_front()?;
            bytes += item.bytes;
            state.bytes = state.bytes.saturating_sub(item.bytes);
            batch.push(item);
        }
        state.oldest_queued_at = if state.queue.is_empty() {
            None
        } else {
            Some(Instant::now())
        };
        state.in_flight += 1;
        Some(batch)
    }

    /// Sends one batch and accounts for it, whatever happens.
    fn send(&self, batch: Vec<Item>) -> Result<LogsAck, Error> {
        let result = self.send_batch(batch);
        {
            let mut state = self.lock();
            state.in_flight = state.in_flight.saturating_sub(1);
        }
        self.signal.notify_all();
        result
    }

    fn send_batch(&self, batch: Vec<Item>) -> Result<LogsAck, Error> {
        if batch.is_empty() {
            return Ok(LogsAck::default());
        }
        let environment = batch[0].environment.clone();
        let records: Vec<Map<String, Value>> =
            batch.iter().map(|item| item.record.clone()).collect();

        // Counted before the answer is known: an operator watching an outage needs to see the
        // requests that failed, not only the ones that worked.
        self.lock().stats.requests += 1;

        match self.api.post_logs(&environment, &records) {
            Ok(ack) => {
                let mut state = self.lock();
                state.stats.accepted += ack.accepted as u64;
                state.stats.duplicates += ack.duplicates as u64;
                state.stats.rejected += ack.rejected.len() as u64;
                state.attempts = 0;
                state.next_attempt = None;
                drop(state);

                if !ack.rejected.is_empty() {
                    self.logger.say(format!(
                        "PromptOn refused {} of {} monitoring logs: {}",
                        ack.rejected.len(),
                        records.len(),
                        ack.rejected
                            .iter()
                            .take(3)
                            .map(|rejected| format!(
                                "[{}] {}: {}",
                                rejected.index, rejected.code, rejected.message
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ));
                }
                Ok(ack)
            }
            Err(failure) => {
                let status = failure.status;
                if status == Some(413) && batch.len() > 1 {
                    let half = batch.len() / 2;
                    let second = batch[half..].to_vec();
                    let first = batch[..half].to_vec();
                    let mut state = self.lock();
                    state.pending.push_front(second);
                    state.pending.push_front(first);
                    drop(state);
                    self.logger.say(format!(
                        "a batch of {} monitoring logs was too large; splitting it in half",
                        records.len()
                    ));
                    return Err(failure.error);
                }

                if failure.is_retryable() {
                    let mut state = self.lock();
                    state.attempts = state.attempts.saturating_add(1);
                    let attempts = state.attempts;
                    if attempts > self.config.log.max_attempts {
                        state.stats.dropped_undeliverable += batch.len() as u64;
                        state.attempts = 0;
                        state.next_attempt = None;
                        drop(state);
                        self.logger.say(format!(
                            "dropping {} monitoring logs after {} failed attempts: {}",
                            records.len(),
                            attempts - 1,
                            failure.error
                        ));
                        return Err(failure.error);
                    }
                    let wait = failure
                        .retry_after
                        .unwrap_or_else(|| retry_backoff(attempts));
                    state.next_attempt = Some(Instant::now() + wait);
                    state.pending.push_front(batch);
                    drop(state);
                    self.logger.say(format!(
                        "monitoring log send failed ({}); retrying the same batch in {}s",
                        failure.error,
                        wait.as_secs().max(1)
                    ));
                    return Err(failure.error);
                }

                let mut state = self.lock();
                state.stats.dropped_undeliverable += batch.len() as u64;
                drop(state);
                self.logger.say_once(
                    "logs-4xx",
                    format!(
                        "PromptOn refused a batch of {} monitoring logs and it will not be retried: {}",
                        records.len(),
                        failure.error
                    ),
                );
                Err(failure.error)
            }
        }
    }

    /// Stops the background thread after one last best-effort drain, bounded by
    /// [`SHUTDOWN_DRAIN`].
    pub fn stop(&self) {
        {
            let mut state = self.lock();
            state.stop = true;
            state.stopped_at.get_or_insert_with(Instant::now);
        }
        self.signal.notify_all();
    }

    /// The flush loop: waits for a trigger, honours the retry pause, and sends one batch at a
    /// time.
    pub fn run_worker(buffer: Arc<Buffer>) {
        enum Action {
            Send,
            Wait(Duration),
            Stop,
        }

        loop {
            // `stopping` records the value of `stop` the action was decided under, so the
            // wait below can tell "nothing changed, go to sleep" from "stop() arrived while the
            // lock was released, decide again".
            let (action, stopping) = {
                let state = buffer.lock();
                let now = Instant::now();
                // A batch that failed asked to be left alone until `next_attempt`.
                let pause = state.next_attempt.filter(|next| *next > now);

                let action = if state.stop {
                    // Shutdown drains what can go out right now. It never waits a retry pause
                    // out and never sends into one, and it gives up after SHUTDOWN_DRAIN — so
                    // dropping the last Client returns in milliseconds even when PromptOn is
                    // unhealthy, instead of spinning through a 4-minute backoff ladder.
                    let spent = state
                        .stopped_at
                        .map(|at| now.saturating_duration_since(at) >= SHUTDOWN_DRAIN)
                        .unwrap_or(false);
                    if state.in_flight > 0 {
                        Action::Wait(Duration::from_millis(20))
                    } else if pause.is_some() || spent || state.total() == 0 {
                        Action::Stop
                    } else {
                        Action::Send
                    }
                } else if let Some(next) = pause {
                    Action::Wait(next.saturating_duration_since(now))
                } else if state.total() == 0 {
                    Action::Wait(buffer.config.log.flush_interval)
                } else if !state.pending.is_empty()
                    || state.queue.len() >= buffer.config.log.flush_size
                    || state.bytes >= buffer.config.log.flush_bytes
                {
                    Action::Send
                } else {
                    match state.oldest_queued_at {
                        Some(at) => {
                            let due = at + buffer.config.log.flush_interval;
                            if due <= now {
                                Action::Send
                            } else {
                                Action::Wait(due.saturating_duration_since(now))
                            }
                        }
                        None => Action::Send,
                    }
                };
                (action, state.stop)
            };

            match action {
                Action::Stop => return,
                Action::Send => {
                    if let Some(batch) = buffer.take_batch(true) {
                        // A failure is already logged and requeued inside send().
                        let _ = buffer.send(batch);
                    }
                }
                Action::Wait(wait) => {
                    let state = buffer.lock();
                    if state.stop != stopping {
                        continue;
                    }
                    let _ = buffer.signal.wait_timeout(state, wait);
                }
            }
        }
    }
}

/// `1 s × 2^(attempt-1)`, capped at five minutes.
pub(crate) fn retry_backoff(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(20);
    RETRY_BASE
        .checked_mul(1u32 << exponent)
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_doubles_to_five_minutes() {
        assert_eq!(retry_backoff(1), Duration::from_secs(1));
        assert_eq!(retry_backoff(2), Duration::from_secs(2));
        assert_eq!(retry_backoff(5), Duration::from_secs(16));
        assert_eq!(retry_backoff(20), MAX_BACKOFF);
    }
}
