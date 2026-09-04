//! The snapshot store: three tiers, one background refresh, and a rule that a generation never
//! fails because PromptOn did.
//!
//! * **Memory** holds the last good document and answers every resolve, with no HTTP call inside
//!   the cache TTL (10 s by default).
//! * **Disk** is written atomically (temp file, then rename) with a sidecar holding the ETag and
//!   `Last-Modified`, and is loaded at start before the first poll returns.
//! * **Bundle** is a snapshot file committed into the app, used when memory and disk are empty —
//!   in a serverless runtime it is the primary cold-start fallback, not a nicety.
//!
//! Load order on start is memory → disk → bundle → remote, and the source is reported as
//! `remote | disk | bundle` on every monitoring log. A document for another environment or
//! project is never used. A refresh that fails, times out or is rate-limited leaves the previous
//! document in place and the caller never sees an error; only when no tier has a document does
//! resolution fail.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::api::{Api, SnapshotFetch};
use crate::config::{Config, MAX_BACKOFF};
use crate::error::Error;
use crate::logger::Logger;
use crate::resolver::{self, Resolution, ResolutionSource, ResolveOptions};
use crate::snapshot::SnapshotDocument;

/// What the store currently holds, for dashboards and health checks.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotInfo {
    /// Which tier the document came from; `None` when there is no document at all.
    pub source: Option<ResolutionSource>,
    /// The document's ETag.
    pub etag: Option<String>,
    /// The document's `Last-Modified`.
    pub last_modified: Option<String>,
    /// The project the document belongs to.
    pub project: Option<String>,
    /// The environment the document belongs to.
    pub environment: Option<String>,
    /// Its schema version.
    pub schema_version: Option<i64>,
    /// When the SDK obtained it.
    pub fetched_at: Option<SystemTime>,
    /// How long ago the last successful refresh was.
    pub age: Option<Duration>,
    /// Whether the last refresh failed, or the document did not come from PromptOn.
    pub stale: bool,
    /// How many refreshes have failed in a row.
    pub failures: u32,
}

#[derive(Debug)]
pub(crate) struct Entry {
    pub document: Arc<SnapshotDocument>,
    pub raw: Arc<Vec<u8>>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub source: ResolutionSource,
    pub fetched_at: SystemTime,
    pub refreshed_at: Instant,
    pub stale: bool,
}

#[derive(Debug, Default)]
struct State {
    entry: Option<Arc<Entry>>,
    failures: u32,
    next_attempt: Option<Instant>,
    stop: bool,
}

pub(crate) struct Store {
    config: Arc<Config>,
    api: Arc<Api>,
    logger: Logger,
    state: Mutex<State>,
    signal: Condvar,
    fetches: AtomicU64,
    refreshing: AtomicBool,
}

impl Store {
    pub fn new(config: Arc<Config>, api: Arc<Api>, logger: Logger) -> Store {
        Store {
            config,
            api,
            logger,
            state: Mutex::new(State::default()),
            signal: Condvar::new(),
            fetches: AtomicU64::new(0),
            refreshing: AtomicBool::new(false),
        }
    }

    /// Loads the disk cache, then the bundle. Called once at start, before the first poll.
    pub fn load_local(&self) {
        if self.current().is_some() {
            return;
        }
        if let Some(path) = self.config.disk_cache_path() {
            if self.load_file(&path, ResolutionSource::Disk) {
                return;
            }
        }
        if let Some(path) = self.config.bundle.clone() {
            self.load_file(&path, ResolutionSource::Bundle);
        }
    }

    fn load_file(&self, path: &Path, source: ResolutionSource) -> bool {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(error) => {
                self.logger.say(format!(
                    "could not read the {source:?} snapshot {path:?}: {error}"
                ));
                return false;
            }
        };

        // A partially written or corrupt file is ignored, not an error: the next poll fixes it.
        let (document, warnings) = match SnapshotDocument::from_json(&bytes) {
            Ok(decoded) => decoded,
            Err(error) => {
                self.logger.say(format!(
                    "ignoring the {source:?} snapshot {path:?}: {error}"
                ));
                return false;
            }
        };
        for warning in warnings {
            self.logger.say(format!("snapshot {path:?}: {warning}"));
        }
        if let Err(reason) = self.accept(&document) {
            self.logger.say(format!(
                "refusing the {source:?} snapshot {path:?}: {reason}"
            ));
            return false;
        }

        let meta = read_sidecar(path);
        let entry = Entry {
            document: Arc::new(document),
            raw: Arc::new(bytes),
            etag: meta.get("etag").and_then(Value::as_str).map(str::to_string),
            last_modified: meta
                .get("last_modified")
                .and_then(Value::as_str)
                .map(str::to_string),
            source,
            fetched_at: SystemTime::now(),
            // A file is stale by definition until the server confirms it.
            refreshed_at: Instant::now(),
            stale: true,
        };
        self.install(entry);
        self.logger.say(format!(
            "loaded the snapshot from {} ({})",
            source.as_str(),
            path.display()
        ));
        true
    }

    /// A document for another environment or project is never used.
    fn accept(&self, document: &SnapshotDocument) -> Result<(), String> {
        if let Some(environment) = &document.environment {
            if environment != &self.config.environment {
                return Err(format!(
                    "it is for environment {environment:?}, this process reads {:?}",
                    self.config.environment
                ));
            }
        }
        match (&document.project, &self.config.project) {
            (Some(project), Some(expected)) if project != expected => Err(format!(
                "it is for project {project:?}, this key is for {expected:?}"
            )),
            _ => Ok(()),
        }
    }

    pub fn current(&self) -> Option<Arc<Entry>> {
        self.state.lock().ok()?.entry.clone()
    }

    fn install(&self, entry: Entry) {
        if let Ok(mut state) = self.state.lock() {
            state.entry = Some(Arc::new(entry));
        }
    }

    /// Replaces the document from the app's own hands (test mode, or a manual override).
    pub fn put_document(&self, document: SnapshotDocument, raw: Vec<u8>, source: ResolutionSource) {
        self.install(Entry {
            document: Arc::new(document),
            raw: Arc::new(raw),
            etag: None,
            last_modified: None,
            source,
            fetched_at: SystemTime::now(),
            refreshed_at: Instant::now(),
            stale: false,
        });
    }

    /// Resolves against the document in memory, triggering a background refresh when it is older
    /// than the cache TTL. Never blocks on the network.
    pub fn resolve(&self, use_case: &str, options: &ResolveOptions) -> Result<Resolution, Error> {
        let entry = self.current().ok_or_else(|| {
            Error::NotReady(format!(
                "no snapshot for environment {:?}",
                self.config.environment
            ))
        })?;

        if entry.refreshed_at.elapsed() >= self.config.cache_ttl {
            self.wake();
        }

        resolver::resolve(
            &entry.document,
            use_case,
            options,
            entry.source,
            entry.etag.as_deref(),
        )
    }

    /// The payload policy the snapshot carries for a use case, when it has one.
    pub fn payload_policy(&self, use_case: &str) -> Option<crate::snapshot::PayloadPolicy> {
        self.current()?
            .document
            .use_cases
            .get(use_case)?
            .payload_policy
            .clone()
    }

    /// Wakes the poller so it can refresh now, if its own rate limit allows.
    pub fn wake(&self) {
        self.signal.notify_all();
    }

    /// Whether the document in memory is older than the cache TTL and the SDK is allowed to ask
    /// for a new one right now (no `Retry-After` or backoff still running).
    pub fn needs_refresh(&self) -> bool {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Instant::now();
        if state.next_attempt.is_some_and(|next| next > now) {
            return false;
        }
        match &state.entry {
            None => true,
            Some(entry) => entry.refreshed_at.elapsed() >= self.config.cache_ttl,
        }
    }

    /// Refreshes on a one-shot thread, at most one at a time. This is the stale-while-revalidate
    /// path for a client running without the background poller: the caller returns immediately
    /// with the document it already has.
    pub fn refresh_in_background(store: Arc<Store>) {
        if store
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("prompton-refresh".to_string())
            .spawn(move || {
                let _ = store.fetch();
                store.refreshing.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            // The thread could not start: leave the flag clear so the next resolve tries again.
        }
    }

    /// Fetches once, synchronously, and installs the result. This is the "fetch now" scripts want;
    /// everything else refreshes in the background.
    pub fn refresh_now(&self) -> Result<(), Error> {
        if !self.config.remote_enabled() {
            return Err(Error::RemoteDisabled(format!(
                "mode is {:?} and api_key is {}",
                self.config.mode,
                if self.config.api_key.is_some() {
                    "set"
                } else {
                    "unset"
                }
            )));
        }
        self.fetch()
    }

    fn fetch(&self) -> Result<(), Error> {
        let etag = self.current().and_then(|entry| entry.etag.clone());
        self.fetches.fetch_add(1, Ordering::Relaxed);

        match self.api.snapshot(&self.config.environment, etag.as_deref()) {
            Ok(SnapshotFetch::NotModified {
                etag,
                last_modified,
            }) => {
                self.confirm(etag, last_modified);
                Ok(())
            }
            Ok(SnapshotFetch::Fetched {
                body,
                etag,
                last_modified,
            }) => {
                let (document, warnings) = match SnapshotDocument::from_json(&body) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        self.record_failure(None);
                        self.logger.say(format!(
                            "snapshot fetch returned an unusable document: {error}"
                        ));
                        return Err(Error::Decode(error));
                    }
                };
                for warning in warnings {
                    self.logger
                        .say_once(&warning, format!("snapshot: {warning}"));
                }
                if let Err(reason) = self.accept(&document) {
                    self.record_failure(None);
                    self.logger
                        .say(format!("refusing the fetched snapshot: {reason}"));
                    return Err(Error::Config(reason));
                }

                let entry = Entry {
                    document: Arc::new(document),
                    raw: Arc::new(body),
                    etag: etag.clone(),
                    last_modified: last_modified.clone(),
                    source: ResolutionSource::Remote,
                    fetched_at: SystemTime::now(),
                    refreshed_at: Instant::now(),
                    stale: false,
                };
                let raw = entry.raw.clone();
                self.install(entry);
                if let Ok(mut state) = self.state.lock() {
                    state.failures = 0;
                    state.next_attempt = None;
                }
                self.write_disk_cache(&raw, etag.as_deref(), last_modified.as_deref());
                Ok(())
            }
            Err(failure) => {
                let retry_after = failure.retry_after;
                self.record_failure(retry_after);
                let wait = self
                    .state
                    .lock()
                    .ok()
                    .and_then(|state| state.next_attempt)
                    .map(|at| at.saturating_duration_since(Instant::now()))
                    .unwrap_or_default();
                self.logger.say(format!(
                    "snapshot refresh failed ({}); serving the cached document, next attempt in {}s",
                    failure.error,
                    wait.as_secs()
                ));
                Err(failure.error)
            }
        }
    }

    fn confirm(&self, etag: Option<String>, last_modified: Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.failures = 0;
            state.next_attempt = None;
            if let Some(entry) = &state.entry {
                state.entry = Some(Arc::new(Entry {
                    document: entry.document.clone(),
                    raw: entry.raw.clone(),
                    etag: etag.or_else(|| entry.etag.clone()),
                    last_modified: last_modified.or_else(|| entry.last_modified.clone()),
                    // The server confirmed the document we hold is current, so it is no longer a
                    // disk or bundle guess: it is what PromptOn serves.
                    source: ResolutionSource::Remote,
                    fetched_at: SystemTime::now(),
                    refreshed_at: Instant::now(),
                    stale: false,
                }));
            }
        }
    }

    fn record_failure(&self, retry_after: Option<Duration>) {
        if let Ok(mut state) = self.state.lock() {
            state.failures = state.failures.saturating_add(1);
            let wait =
                retry_after.unwrap_or_else(|| backoff(self.config.cache_ttl, state.failures));
            state.next_attempt = Some(Instant::now() + wait);
            if let Some(entry) = &state.entry {
                if !entry.stale {
                    state.entry = Some(Arc::new(Entry {
                        document: entry.document.clone(),
                        raw: entry.raw.clone(),
                        etag: entry.etag.clone(),
                        last_modified: entry.last_modified.clone(),
                        source: entry.source,
                        fetched_at: entry.fetched_at,
                        refreshed_at: entry.refreshed_at,
                        stale: true,
                    }));
                }
            }
        }
    }

    /// How many times the store has asked the server for a snapshot (tests assert on this).
    pub fn fetch_count(&self) -> u64 {
        self.fetches.load(Ordering::Relaxed)
    }

    pub fn info(&self) -> SnapshotInfo {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        match &state.entry {
            None => SnapshotInfo {
                source: None,
                etag: None,
                last_modified: None,
                project: None,
                environment: None,
                schema_version: None,
                fetched_at: None,
                age: None,
                stale: true,
                failures: state.failures,
            },
            Some(entry) => SnapshotInfo {
                source: Some(entry.source),
                etag: entry.etag.clone(),
                last_modified: entry.last_modified.clone(),
                project: entry.document.project.clone(),
                environment: entry.document.environment.clone(),
                schema_version: Some(entry.document.schema_version),
                fetched_at: Some(entry.fetched_at),
                age: Some(entry.refreshed_at.elapsed()),
                stale: entry.stale || entry.source != ResolutionSource::Remote,
                failures: state.failures,
            },
        }
    }

    /// Writes the current document to `path` (plus its sidecar) so it can be committed as a
    /// bundle.
    pub fn export(&self, path: &Path) -> Result<(), Error> {
        let entry = self
            .current()
            .ok_or_else(|| Error::NotReady("nothing to export".to_string()))?;
        write_atomically(path, &entry.raw)?;
        write_atomically(
            &sidecar_path(path),
            sidecar_json(
                entry.etag.as_deref(),
                entry.last_modified.as_deref(),
                entry.document.environment.as_deref(),
                entry.document.project.as_deref(),
            )
            .as_bytes(),
        )?;
        Ok(())
    }

    fn write_disk_cache(&self, body: &[u8], etag: Option<&str>, last_modified: Option<&str>) {
        let path = match self.config.disk_cache_path() {
            Some(path) => path,
            None => return,
        };
        let document_environment = Some(self.config.environment.as_str());
        let result = write_atomically(&path, body).and_then(|()| {
            write_atomically(
                &sidecar_path(&path),
                sidecar_json(
                    etag,
                    last_modified,
                    document_environment,
                    self.config.project.as_deref(),
                )
                .as_bytes(),
            )
        });
        if let Err(error) = result {
            self.logger.say_once(
                "disk-cache-write",
                format!("could not write the disk cache {}: {error}", path.display()),
            );
        }
    }

    /// Stops the poller.
    pub fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stop = true;
        }
        self.signal.notify_all();
    }

    /// The poll loop: refresh when the TTL has passed, wait out backoff and `Retry-After`, and
    /// never let a failure escape.
    pub fn run_poller(store: Arc<Store>) {
        loop {
            let state = match store.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            if state.stop {
                return;
            }

            let now = Instant::now();
            let due = match &state.entry {
                None => now,
                Some(entry) => entry.refreshed_at + store.config.cache_ttl,
            };
            let due = match state.next_attempt {
                Some(next) if next > due => next,
                _ => due,
            };

            let wait = due.saturating_duration_since(now);
            if wait > Duration::ZERO {
                let (state, _) = match store.signal.wait_timeout(state, wait) {
                    Ok(result) => result,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if state.stop {
                    return;
                }
                continue;
            }

            drop(state);
            // A refresh never blocks or fails a generation: the error is logged inside fetch().
            let _ = store.fetch();
        }
    }
}

/// `cache_ttl × 2^(failures-1)`, capped at five minutes.
pub(crate) fn backoff(base: Duration, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(20);
    let factor = 1u32 << exponent.min(20);
    base.checked_mul(factor)
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".meta.json");
    PathBuf::from(name)
}

fn sidecar_json(
    etag: Option<&str>,
    last_modified: Option<&str>,
    environment: Option<&str>,
    project: Option<&str>,
) -> String {
    json!({
        "etag": etag,
        "last_modified": last_modified,
        "environment": environment,
        "project": project,
        "fetched_at": crate::record::now_iso8601(),
    })
    .to_string()
}

fn read_sidecar(path: &Path) -> Value {
    fs::read(sidecar_path(path))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

/// Writes `contents` to `path` through a temp file and a rename, so a reader either sees the old
/// file or the new one — never half of either.
pub(crate) fn write_atomically(path: &Path, contents: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut temp = path.as_os_str().to_os_string();
    temp.push(format!(".tmp.{}.{unique}", std::process::id()));
    let temp = PathBuf::from(temp);

    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_stops_at_five_minutes() {
        let ttl = Duration::from_secs(10);
        assert_eq!(backoff(ttl, 1), Duration::from_secs(10));
        assert_eq!(backoff(ttl, 2), Duration::from_secs(20));
        assert_eq!(backoff(ttl, 5), Duration::from_secs(160));
        assert_eq!(backoff(ttl, 9), MAX_BACKOFF);
        assert_eq!(backoff(ttl, 99), MAX_BACKOFF);
    }

    #[test]
    fn writes_atomically_and_names_the_sidecar() {
        let dir = std::env::temp_dir().join(format!("prompton-store-{}", std::process::id()));
        let path = dir.join("snapshot.json");
        write_atomically(&path, b"{}").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
        assert_eq!(sidecar_path(&path), dir.join("snapshot.json.meta.json"));
        let _ = fs::remove_dir_all(&dir);
    }
}
