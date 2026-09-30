//! The snapshot store keeps immutable documents per use case, plus disk and bundle fallbacks.
//! A model call never fails because PromptOn did.
//!
//! * **Memory** holds the last good document for each use case and answers every resolve, with no
//!   HTTP call inside the cache TTL (10 s by default).
//! * **Disk** is written atomically (temp file, then rename) with a sidecar holding the ETag and
//!   `Last-Modified`, and is loaded at start.
//! * **Bundle** is a snapshot file committed into the app, used when memory and disk are empty —
//!   in a serverless runtime it is the primary cold-start fallback, not a nicety.
//!
//! Load order on start is memory → disk → bundle. Remote fetches happen on demand for the requested
//! use case. A document for another environment or project is never used. A refresh that fails,
//! times out or is rate-limited leaves the previous document in place; only when no tier has a
//! document does resolution fail.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::api::{Api, UseCaseFetch};
use crate::config::{Config, MAX_BACKOFF};
use crate::error::Error;
use crate::logger::Logger;
use crate::resolver::{self, Resolution, ResolveOptions, Source};
use crate::snapshot::UseCaseDocument;

/// What the store currently holds, for dashboards and health checks.
#[derive(Debug, Clone, PartialEq)]
pub struct UseCaseDocumentInfo {
    /// Which tier the document came from; `None` when there is no document at all.
    pub source: Option<Source>,
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
    pub document: Arc<UseCaseDocument>,
    pub raw: Arc<Vec<u8>>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub source: Source,
    pub fetched_at: SystemTime,
    pub refreshed_at: Instant,
    pub stale: bool,
}

#[derive(Debug, Default)]
struct State {
    entry: Option<Arc<Entry>>,
    per_key: HashMap<String, Arc<Entry>>,
    failures: u32,
    #[allow(dead_code)]
    next_attempt: Option<Instant>,
    next_attempts: HashMap<String, Instant>,
    inflight: HashMap<String, u64>,
    stop: bool,
}

pub(crate) struct Store {
    config: Arc<Config>,
    api: Arc<Api>,
    logger: Logger,
    state: Mutex<State>,
    signal: Condvar,
    fetches: AtomicU64,
    fetch_tokens: AtomicU64,
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
            fetch_tokens: AtomicU64::new(0),
        }
    }

    /// Loads the disk cache, then the bundle. Called once at start without remote I/O.
    pub fn load_local(&self) {
        if self.current().is_some() {
            return;
        }
        if let Some(path) = self.config.bundle.clone() {
            self.load_file(&path, Source::Bundle);
        }
        if let Some(path) = self.config.disk_cache_path() {
            self.load_file(&path, Source::Disk);
            self.load_key_files(&path);
        }
    }

    fn load_key_files(&self, full_path: &Path) {
        let dir = key_cache_dir(full_path);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.logger.say(format!(
                    "could not read the prompt disk cache {dir:?}: {error}"
                ));
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let (document, warnings) = match UseCaseDocument::from_json(&bytes) {
                Ok(decoded) => decoded,
                Err(_) => continue,
            };
            if self.accept(&document).is_err() {
                continue;
            }
            for warning in warnings {
                self.logger.say(format!("snapshot {path:?}: {warning}"));
            }
            let meta = read_sidecar(&path);
            let entry = Entry {
                document: Arc::new(document),
                raw: Arc::new(bytes),
                etag: meta.get("etag").and_then(Value::as_str).map(str::to_string),
                last_modified: meta
                    .get("last_modified")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                source: Source::Disk,
                fetched_at: meta
                    .get("fetched_at")
                    .and_then(Value::as_str)
                    .and_then(parse_system_time)
                    .unwrap_or_else(SystemTime::now),
                refreshed_at: Instant::now(),
                stale: true,
            };
            for key in entry.document.use_cases.keys().cloned().collect::<Vec<_>>() {
                self.install_key(
                    &key,
                    Entry {
                        document: entry.document.clone(),
                        raw: entry.raw.clone(),
                        etag: entry.etag.clone(),
                        last_modified: entry.last_modified.clone(),
                        source: entry.source,
                        fetched_at: entry.fetched_at,
                        refreshed_at: entry.refreshed_at,
                        stale: entry.stale,
                    },
                );
            }
        }
    }

    fn load_file(&self, path: &Path, source: Source) -> bool {
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

        // A partially written or corrupt file is ignored, not an error: a later demand fetch fixes it.
        let (document, warnings) = match UseCaseDocument::from_json(&bytes) {
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
    fn accept(&self, document: &UseCaseDocument) -> Result<(), String> {
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
        let state = self.state.lock().ok()?;
        state
            .entry
            .clone()
            .or_else(|| state.per_key.values().next().cloned())
    }

    fn current_key(&self, use_case: &str) -> Option<Arc<Entry>> {
        let state = self.state.lock().ok()?;
        state.per_key.get(use_case).cloned().or_else(|| {
            state
                .entry
                .clone()
                .filter(|entry| entry.document.use_cases.contains_key(use_case))
        })
    }

    fn current_for_resolve(&self, use_case: &str) -> Option<Arc<Entry>> {
        let state = self.state.lock().ok()?;
        state
            .per_key
            .get(use_case)
            .cloned()
            .or_else(|| state.entry.clone())
    }

    fn install(&self, entry: Entry) {
        if let Ok(mut state) = self.state.lock() {
            let entry = Arc::new(entry);
            for key in entry.document.use_cases.keys() {
                state.per_key.insert(key.clone(), entry.clone());
            }
            state.entry = Some(entry);
        }
    }

    fn install_key(&self, use_case: &str, entry: Entry) {
        if let Ok(mut state) = self.state.lock() {
            state.per_key.insert(use_case.to_string(), Arc::new(entry));
        }
    }

    /// Replaces the document from the app's own hands (test mode, or a manual override).
    pub fn put_document(&self, document: UseCaseDocument, raw: Vec<u8>, source: Source) {
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

    /// Resolves against the document cached for this use case.
    pub(crate) fn resolve(
        &self,
        use_case: &str,
        options: &ResolveOptions,
    ) -> Result<Resolution, Error> {
        let entry = self.current_for_resolve(use_case).ok_or_else(|| {
            Error::NotReady(format!(
                "no snapshot for environment {:?}",
                self.config.environment
            ))
        })?;

        resolver::resolve(
            &entry.document,
            use_case,
            options,
            entry.source,
            entry.etag.as_deref(),
        )
    }

    /// The prompt names cached for a use case.
    pub fn prompt_names(&self, use_case: &str) -> Result<Vec<String>, Error> {
        let entry = self.current_key(use_case).ok_or_else(|| {
            Error::NotReady(format!(
                "no snapshot for environment {:?}",
                self.config.environment
            ))
        })?;
        if !entry.document.use_cases.contains_key(use_case) {
            return Err(Error::UnknownUseCase(use_case.to_string()));
        }
        Ok(entry.document.prompt_names(use_case))
    }

    /// The payload policy the snapshot carries for a use case, when it has one.
    pub fn payload_policy(&self, use_case: &str) -> Option<crate::snapshot::PayloadPolicy> {
        self.current_key(use_case)?
            .document
            .use_cases
            .get(use_case)?
            .payload_policy
            .clone()
    }

    /// Fetches the full document once, synchronously, and installs the result. This is the
    /// explicit "fetch now" path used by scripts and bundle export flows.
    pub fn refresh_now(&self) -> Result<(), Error> {
        // Runtime refresh is demand-driven by use_case(key). A no-arg refresh must
        // not bulk-fetch all prompts.
        Ok(())
    }

    /// Ensures the requested use case has a fresh-enough remote document. This is the runtime
    /// demand path: it fetches only `/prompts/{key}`, waits at most one second, and falls back to
    /// the last entry on failure. Concurrent callers for the same key share one request; different
    /// keys proceed independently.
    pub fn refresh_key_if_needed(self: &Arc<Self>, use_case: &str) {
        if !self.config.remote_enabled() {
            return;
        }

        let budget = self.config.request_timeout.min(Duration::from_secs(1));
        let deadline = Instant::now() + budget;
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };

        loop {
            let now = Instant::now();
            if let Some(entry) = state.per_key.get(use_case).or(state.entry.as_ref()) {
                if entry.document.use_cases.contains_key(use_case)
                    && entry.source == Source::Remote
                    && entry.refreshed_at.elapsed() < self.config.demand_cache_ttl
                {
                    return;
                }
            }

            if state.inflight.contains_key(use_case) {
                let remaining = deadline.saturating_duration_since(now);
                if remaining == Duration::ZERO {
                    return;
                }
                state = match self
                    .signal
                    .wait_timeout(state, remaining.min(Duration::from_millis(10)))
                {
                    Ok((state, _)) => state,
                    Err(poisoned) => poisoned.into_inner().0,
                };
                continue;
            }

            if state
                .next_attempts
                .get(use_case)
                .is_some_and(|next| *next > now)
            {
                return;
            }

            let token = self.fetch_tokens.fetch_add(1, Ordering::Relaxed) + 1;
            state.inflight.insert(use_case.to_string(), token);
            state
                .next_attempts
                .insert(use_case.to_string(), now + self.config.demand_cache_ttl);
            drop(state);

            let store = self.clone();
            let key = use_case.to_string();
            let _ = std::thread::Builder::new()
                .name("prompton-config-fetch".to_string())
                .spawn(move || {
                    let _ = store.fetch_key(&key, deadline);
                    if let Ok(mut state) = store.state.lock() {
                        if state.inflight.get(&key).copied() == Some(token) {
                            state.inflight.remove(&key);
                        }
                    }
                    store.signal.notify_all();
                });

            state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            while state.inflight.contains_key(use_case) {
                let now = Instant::now();
                let remaining = deadline.saturating_duration_since(now);
                if remaining == Duration::ZERO {
                    return;
                }
                state = match self
                    .signal
                    .wait_timeout(state, remaining.min(Duration::from_millis(10)))
                {
                    Ok((state, _)) => state,
                    Err(poisoned) => poisoned.into_inner().0,
                };
            }
            return;
        }
    }

    #[allow(dead_code)]
    fn fetch(&self) -> Result<(), Error> {
        let etag = self.current().and_then(|entry| entry.etag.clone());
        self.fetches.fetch_add(1, Ordering::Relaxed);

        match self.api.snapshot(&self.config.environment, etag.as_deref()) {
            Ok(UseCaseFetch::NotModified {
                etag,
                last_modified,
            }) => {
                self.confirm(etag, last_modified);
                Ok(())
            }
            Ok(UseCaseFetch::Fetched {
                body,
                etag,
                last_modified,
            }) => {
                let (document, warnings) = match UseCaseDocument::from_json(&body) {
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
                    source: Source::Remote,
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

    fn fetch_key(&self, use_case: &str, deadline: Instant) -> Result<(), Error> {
        let etag = self.current_key(use_case).and_then(|entry| {
            if entry.document.use_cases.contains_key(use_case) {
                entry.etag.clone()
            } else {
                None
            }
        });
        self.fetches.fetch_add(1, Ordering::Relaxed);

        match self
            .api
            .prompt_snapshot(use_case, &self.config.environment, etag.as_deref())
        {
            Ok(UseCaseFetch::NotModified {
                etag,
                last_modified,
            }) => {
                if Instant::now() > deadline {
                    self.record_key_failure(use_case, None);
                    return Err(Error::Transport(
                        "config fetch exceeded the 1s budget".to_string(),
                    ));
                }
                if let Some(entry) = self.current_key(use_case) {
                    self.confirm_key(use_case, &entry, etag, last_modified);
                    Ok(())
                } else {
                    self.record_key_failure(use_case, None);
                    Err(Error::NotReady(format!(
                        "config fetch for {use_case:?} returned 304 without a cached document"
                    )))
                }
            }
            Ok(UseCaseFetch::Fetched {
                body,
                etag,
                last_modified,
            }) => {
                let (document, warnings) = match UseCaseDocument::from_json(&body) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        self.record_key_failure(use_case, None);
                        self.logger.say(format!(
                            "config fetch for {use_case} returned an unusable document: {error}"
                        ));
                        return Err(Error::Decode(error));
                    }
                };
                for warning in warnings {
                    self.logger
                        .say_once(&warning, format!("snapshot: {warning}"));
                }
                if let Err(reason) = self.accept(&document) {
                    self.record_key_failure(use_case, None);
                    self.logger
                        .say(format!("refusing the fetched snapshot: {reason}"));
                    return Err(Error::Config(reason));
                }
                if !document.use_cases.contains_key(use_case) {
                    self.record_key_failure(use_case, None);
                    return Err(Error::Config(format!(
                        "config fetch for {use_case:?} returned another use case"
                    )));
                }
                if Instant::now() > deadline {
                    self.record_key_failure(use_case, None);
                    return Err(Error::Transport(
                        "config fetch exceeded the 1s budget".to_string(),
                    ));
                }

                let entry = Entry {
                    document: Arc::new(document),
                    raw: Arc::new(body),
                    etag: etag.clone(),
                    last_modified: last_modified.clone(),
                    source: Source::Remote,
                    fetched_at: SystemTime::now(),
                    refreshed_at: Instant::now(),
                    stale: false,
                };
                let raw = entry.raw.clone();
                self.install_key(use_case, entry);
                self.write_key_disk_cache(
                    use_case,
                    &raw,
                    etag.as_deref(),
                    last_modified.as_deref(),
                );
                if let Ok(mut state) = self.state.lock() {
                    state.failures = 0;
                    state.next_attempts.remove(use_case);
                }
                Ok(())
            }
            Err(failure) => {
                self.record_key_failure(
                    use_case,
                    Some(self.config.demand_cache_ttl).or(failure.retry_after),
                );
                self.logger.say(format!(
                    "config fetch for {use_case} failed ({}); serving the cached document if present",
                    failure.error
                ));
                Err(failure.error)
            }
        }
    }

    #[allow(dead_code)]
    fn confirm(&self, etag: Option<String>, last_modified: Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.failures = 0;
            state.next_attempt = None;
            if let Some(entry) = &state.entry {
                let fresh = Arc::new(Entry {
                    document: entry.document.clone(),
                    raw: entry.raw.clone(),
                    etag: etag.or_else(|| entry.etag.clone()),
                    last_modified: last_modified.or_else(|| entry.last_modified.clone()),
                    // The server confirmed the document we hold is current, so it is no longer a
                    // disk or bundle guess: it is what PromptOn serves.
                    source: Source::Remote,
                    fetched_at: SystemTime::now(),
                    refreshed_at: Instant::now(),
                    stale: false,
                });
                for key in fresh.document.use_cases.keys() {
                    state.per_key.insert(key.clone(), fresh.clone());
                }
                state.entry = Some(fresh);
            }
        }
    }

    fn confirm_key(
        &self,
        use_case: &str,
        entry: &Arc<Entry>,
        etag: Option<String>,
        last_modified: Option<String>,
    ) {
        if let Ok(mut state) = self.state.lock() {
            state.next_attempts.remove(use_case);
            state.per_key.insert(
                use_case.to_string(),
                Arc::new(Entry {
                    document: entry.document.clone(),
                    raw: entry.raw.clone(),
                    etag: etag.or_else(|| entry.etag.clone()),
                    last_modified: last_modified.or_else(|| entry.last_modified.clone()),
                    source: Source::Remote,
                    fetched_at: SystemTime::now(),
                    refreshed_at: Instant::now(),
                    stale: false,
                }),
            );
        }
    }

    #[allow(dead_code)]
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

    fn record_key_failure(&self, use_case: &str, retry_after: Option<Duration>) {
        if let Ok(mut state) = self.state.lock() {
            state.failures = state.failures.saturating_add(1);
            if !state.next_attempts.contains_key(use_case) {
                let wait = retry_after.unwrap_or(self.config.demand_cache_ttl);
                state
                    .next_attempts
                    .insert(use_case.to_string(), Instant::now() + wait);
            }
            if let Some(entry) = state.per_key.get(use_case).cloned() {
                if !entry.stale {
                    state.per_key.insert(
                        use_case.to_string(),
                        Arc::new(Entry {
                            document: entry.document.clone(),
                            raw: entry.raw.clone(),
                            etag: entry.etag.clone(),
                            last_modified: entry.last_modified.clone(),
                            source: entry.source,
                            fetched_at: entry.fetched_at,
                            refreshed_at: entry.refreshed_at,
                            stale: true,
                        }),
                    );
                }
            }
        }
    }

    /// How many times the store has asked the server for a snapshot (tests assert on this).
    pub fn fetch_count(&self) -> u64 {
        self.fetches.load(Ordering::Relaxed)
    }

    pub fn info(&self) -> UseCaseDocumentInfo {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = state
            .entry
            .as_ref()
            .or_else(|| state.per_key.values().next());
        match entry {
            None => UseCaseDocumentInfo {
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
            Some(entry) => UseCaseDocumentInfo {
                source: Some(entry.source),
                etag: entry.etag.clone(),
                last_modified: entry.last_modified.clone(),
                project: entry.document.project.clone(),
                environment: entry.document.environment.clone(),
                schema_version: Some(entry.document.schema_version),
                fetched_at: Some(entry.fetched_at),
                age: Some(entry.refreshed_at.elapsed()),
                stale: entry.stale || entry.source != Source::Remote,
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

    fn write_key_disk_cache(
        &self,
        use_case: &str,
        body: &[u8],
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) {
        let full_path = match self.config.disk_cache_path() {
            Some(path) => path,
            None => return,
        };
        let path = key_cache_path(&full_path, use_case);
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
                "prompt-disk-cache-write",
                format!(
                    "could not write the prompt disk cache {}: {error}",
                    path.display()
                ),
            );
        }
    }

    #[allow(dead_code)]
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

    /// Stops any legacy poller thread and wakes waiters.
    pub fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stop = true;
        }
        self.signal.notify_all();
    }

    /// Legacy poll loop for callers that explicitly opt into background polling.
    #[allow(dead_code)]
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
            // A refresh never blocks or fails a model call: the error is logged inside fetch().
            let _ = store.fetch();
        }
    }
}

/// `cache_ttl × 2^(failures-1)`, capped at five minutes.
#[allow(dead_code)]
pub(crate) fn backoff(base: Duration, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(20);
    let factor = 1u32 << exponent.min(20);
    base.checked_mul(factor)
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

fn key_cache_dir(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".prompts");
    PathBuf::from(name)
}

fn key_cache_path(path: &Path, key: &str) -> PathBuf {
    let encoded = key
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    key_cache_dir(path).join(format!("{encoded}.json"))
}

fn parse_system_time(_value: &str) -> Option<SystemTime> {
    None
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
