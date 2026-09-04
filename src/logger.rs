//! The SDK's own logging: a handful of lines an operator needs to see, on stderr by default.
//!
//! The SDK never takes a logging framework as a dependency. Everything it has to say goes through
//! this type, and an app can redirect it into its own logger with
//! [`crate::ClientBuilder::log_sink`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Where the SDK's log lines go.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// The SDK's logger: a sink plus a memory of which one-time lines have been said.
#[derive(Clone, Default)]
pub struct Logger {
    sink: Option<LogSink>,
    said: Arc<Mutex<HashSet<String>>>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger")
            .field("custom_sink", &self.sink.is_some())
            .finish()
    }
}

impl Logger {
    /// A logger writing to stderr.
    pub fn stderr() -> Logger {
        Logger::default()
    }

    /// A logger writing to `sink`.
    pub fn to_sink(sink: LogSink) -> Logger {
        Logger {
            sink: Some(sink),
            said: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Says one line.
    pub fn say(&self, message: impl AsRef<str>) {
        let message = message.as_ref();
        match &self.sink {
            Some(sink) => sink(message),
            None => eprintln!("[prompton] {message}"),
        }
    }

    /// Says one line the first time this `key` comes up, and stays quiet afterwards.
    pub fn say_once(&self, key: &str, message: impl AsRef<str>) {
        let first = match self.said.lock() {
            Ok(mut said) => said.insert(key.to_string()),
            Err(_) => true,
        };
        if first {
            self.say(message);
        }
    }
}
