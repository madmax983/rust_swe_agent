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
                if let Ok(mut w) = self.writer.lock() {
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
        match self.writer.lock() {
            Ok(mut w) => {
                if w.write_all(line.as_bytes()).is_err() || w.flush().is_err() {
                    self.warn_once("event-log write failed; continuing without event log");
                }
            }
            Err(_) => {
                self.warn_once("event-log writer lock poisoned; continuing without event log");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopens_file_on_request_and_writes_to_new_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("events.jsonl");
        let sink = EventLogSink::new(&path, "mini".into())?;

        // Write initial event
        sink.emit(StreamEvent::RunStarted {
            task: "t1".into(),
            model: "m1".into(),
            started_at: "s1".into(),
        });

        // Rename original file, simulating logrotate
        let rotated_path = dir.path().join("events.jsonl.1");
        std::fs::rename(&path, &rotated_path)?;

        // Request reopen and emit second event
        sink.request_reopen();
        sink.emit(StreamEvent::RunStarted {
            task: "t2".into(),
            model: "m2".into(),
            started_at: "s2".into(),
        });

        // Assert old file has only the first event
        let old_content = std::fs::read_to_string(&rotated_path)?;
        assert_eq!(old_content.lines().count(), 1);
        assert!(old_content.contains("\"task\":\"t1\""));

        // Assert new file was created and has only the second event
        let new_content = std::fs::read_to_string(&path)?;
        assert_eq!(new_content.lines().count(), 1);
        assert!(new_content.contains("\"task\":\"t2\""));

        Ok(())
    }

    #[test]
    fn handles_reopen_failure_gracefully() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("events.jsonl");
        let sink = EventLogSink::new(&path, "mini".into())?;

        // Remove the parent directory so creation of the new file fails
        std::fs::remove_dir_all(dir.path())?;

        sink.request_reopen();
        sink.emit(StreamEvent::RunStarted {
            task: "t1".into(),
            model: "m1".into(),
            started_at: "s1".into(),
        });

        assert_eq!(sink.dropped_reopen_failures.load(Ordering::SeqCst), 1);
        assert!(sink.warned.load(Ordering::SeqCst));

        Ok(())
    }

    #[test]
    fn handles_poisoned_lock_gracefully() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("events.jsonl");
        let sink = EventLogSink::new(&path, "mini".into())?;

        let writer_clone = sink.writer.clone();
        let _ = std::thread::spawn(move || {
            let Ok(_lock) = writer_clone.lock() else {
                panic!("Locking shouldn't fail initially");
            };
            panic!("Poisoning the lock");
        })
        .join();

        // This should not panic
        sink.emit(StreamEvent::RunStarted {
            task: "t1".into(),
            model: "m1".into(),
            started_at: "s1".into(),
        });

        assert!(sink.warned.load(Ordering::SeqCst));

        Ok(())
    }

    #[test]
    fn includes_required_fields() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("events.jsonl");
        let sink = EventLogSink::new(&path, "mini".into())?;
        sink.emit(StreamEvent::RunStarted {
            task: "t".into(),
            model: "m".into(),
            started_at: "s".into(),
        });
        let line = std::fs::read_to_string(path)?;
        let Some(first_line) = line.lines().next() else {
            anyhow::bail!("log file is empty");
        };
        let v: Value = serde_json::from_str(first_line)?;
        assert_eq!(v["schema"], "event-log-v1");
        assert_eq!(v["event_type"], "run_started");
        assert_eq!(v["instance_id"], "mini");
        assert!(v["ts"].as_str().is_some());
        Ok(())
    }
}
