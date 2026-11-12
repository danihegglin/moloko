//! Job logs: written to disk, fanned out live to subscribers.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

pub struct JobLog {
    file: Mutex<BufWriter<File>>,
    path: PathBuf,
    tx: broadcast::Sender<String>,
}

impl JobLog {
    pub fn line(&self, line: &str) {
        // Hold the file lock while broadcasting so subscribe() never misses or duplicates a line.
        let mut f = self.file.lock().unwrap();
        let _ = writeln!(f, "{line}");
        let _ = self.tx.send(line.to_string());
    }
}

#[derive(Default)]
pub struct LogHub {
    dir: PathBuf,
    live: Mutex<HashMap<i64, Arc<JobLog>>>,
}

pub enum Subscription {
    /// Job still running: existing content plus a live stream.
    Live(String, broadcast::Receiver<String>),
    /// Job finished (or never started): full content.
    Done(String),
}

impl LogHub {
    pub fn new(dir: &Path) -> Self {
        LogHub {
            dir: dir.to_path_buf(),
            live: Mutex::default(),
        }
    }

    fn path(&self, job_id: i64) -> PathBuf {
        self.dir.join(format!("{job_id}.log"))
    }

    pub fn open(&self, job_id: i64) -> std::io::Result<Arc<JobLog>> {
        let path = self.path(job_id);
        let file = File::create(&path)?;
        let (tx, _) = broadcast::channel(4096);
        let log = Arc::new(JobLog {
            file: Mutex::new(BufWriter::new(file)),
            path,
            tx,
        });
        self.live.lock().unwrap().insert(job_id, log.clone());
        Ok(log)
    }

    /// Flush and stop streaming. Dropping the sender ends every subscriber's stream.
    pub fn close(&self, job_id: i64) {
        if let Some(log) = self.live.lock().unwrap().remove(&job_id) {
            let _ = log.file.lock().unwrap().flush();
        }
    }

    pub fn subscribe(&self, job_id: i64) -> Subscription {
        let live = self.live.lock().unwrap().get(&job_id).cloned();
        match live {
            Some(log) => {
                let mut f = log.file.lock().unwrap();
                let _ = f.flush();
                let content = std::fs::read_to_string(&log.path).unwrap_or_default();
                let rx = log.tx.subscribe();
                drop(f);
                Subscription::Live(content, rx)
            }
            None => {
                Subscription::Done(std::fs::read_to_string(self.path(job_id)).unwrap_or_default())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn live_then_done() {
        let dir = tempfile::tempdir().unwrap();
        let hub = LogHub::new(dir.path());
        let log = hub.open(1).unwrap();
        log.line("one");
        let Subscription::Live(existing, mut rx) = hub.subscribe(1) else {
            panic!("expected live")
        };
        assert_eq!(existing, "one\n");
        log.line("two");
        assert_eq!(rx.recv().await.unwrap(), "two");
        drop(log);
        hub.close(1);
        assert!(rx.recv().await.is_err());
        let Subscription::Done(all) = hub.subscribe(1) else {
            panic!("expected done")
        };
        assert_eq!(all, "one\ntwo\n");
    }
}
