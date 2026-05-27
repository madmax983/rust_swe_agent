use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::{Map, Value};

use super::{StreamEvent, StreamSink};

#[derive(Clone)]
pub struct EventLogSink {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    path: PathBuf,
    warned: Arc<AtomicBool>,
    reopen_requested: Arc<AtomicBool>,
    dropped_reopen_failures: Arc<AtomicU64>,
    instance_id: String,
}

impl EventLogSink {
    pub fn new(path: &Path, instance_id: String) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Arc::new(Mutex::new(Box::new(file))),
            path: path.to_path_buf(),
            warned: Arc::new(AtomicBool::new(false)),
            reopen_requested: Arc::new(AtomicBool::new(false)),
            dropped_reopen_failures: Arc::new(AtomicU64::new(0)),
            instance_id,
        })
    }
    pub fn request_reopen(&self) {
        self.reopen_requested.store(true, Ordering::SeqCst);
    }

    fn warn_once(&self, message: &str) {
        if !self.warned.swap(true, Ordering::SeqCst) {
            let _ = std::io::stderr().write_all(format!("warn: {message}\n").as_bytes());
        }
    }
}

impl StreamSink for EventLogSink {
    fn emit(&self, event: StreamEvent) {
        if self.reopen_requested.swap(false, Ordering::SeqCst) {
            if let Ok(new_writer) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map(|f| Box::new(f) as Box<dyn Write + Send>)
            {
                if let Ok(mut w) = self.writer.lock().or_else(|e| Ok::<_, ()>(e.into_inner())) {
                    *w = new_writer;
                }
            } else {
                self.dropped_reopen_failures.fetch_add(1, Ordering::SeqCst);
                self.warn_once("event-log reopen failed after SIGHUP; continuing");
            }
        }
        let mut obj = Map::new();
        obj.insert("schema".into(), Value::String("event-log-v1".into()));
        obj.insert("ts".into(), Value::String(Utc::now().to_rfc3339()));
        obj.insert(
            "event_type".into(),
            Value::String(event.event_name().to_owned()),
        );
        obj.insert(
            "instance_id".into(),
            Value::String(self.instance_id.clone()),
        );
        if let Ok(Value::Object(mut payload)) = serde_json::to_value(event) {
            payload.remove("type");
            for (k, v) in payload {
                obj.insert(k, v);
            }
        }
        let line = Value::Object(obj).to_string() + "\n";
        match self.writer.lock().or_else(|e| Ok::<_, ()>(e.into_inner())) {
            Ok(mut w) => {
                if w.write_all(line.as_bytes()).is_err() || w.flush().is_err() {
                    self.warn_once("event-log write failed; continuing without event log");
                }
            }
            Err(()) => {
                self.warn_once("event-log writer lock poisoned; continuing without event log");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn includes_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sink = EventLogSink::new(&path, "mini".into()).unwrap();
        sink.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "s".into(),
        });
        let line = std::fs::read_to_string(path).unwrap();
        let v: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
        assert_eq!(v["schema"], "event-log-v1");
        assert_eq!(v["event_type"], "run_started");
        assert_eq!(v["instance_id"], "mini");
        assert!(v["ts"].as_str().is_some());
    }
}

#[cfg(test)]
mod havoc_event_log_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_event_log_poison_causes_panic_or_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sink = Arc::new(EventLogSink::new(&path, "mini".into()).unwrap());
        let sink_clone = sink.clone();

        let _ = thread::spawn(move || {
            let _lock = sink_clone.writer.lock().unwrap();
            panic!("Intentional poison");
        })
        .join();

        sink.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "s".into(),
        });

        let line = std::fs::read_to_string(path).unwrap();
        assert!(
            !line.is_empty(),
            "Event was silently dropped due to lock poisoning"
        );
    }
}
