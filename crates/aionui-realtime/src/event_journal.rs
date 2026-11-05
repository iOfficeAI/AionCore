//! Durable NDJSON event journal for headless Host resume across process restart (M9 E14).
//!
//! Brand: CSBU WorkMate. No AGPL. Bounded on-disk retention (last `keep` events).

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use aionui_api_types::WebSocketMessage;
use serde_json::Value;
use tracing::warn;

const EPOCH_FILE: &str = "epoch";
const JOURNAL_FILE: &str = "events.ndjson";

/// On-disk companion to [`crate::EventLog`]: persists epoch + recent envelopes.
#[derive(Debug)]
pub struct EventJournal {
    dir: PathBuf,
    keep: usize,
    appends_since_compact: usize,
}

#[derive(Debug, Clone)]
pub struct JournalSnapshot {
    pub epoch: u64,
    pub events: Vec<WebSocketMessage<Value>>,
}

impl EventJournal {
    /// Open (or create) a journal under `dir`. `keep` bounds how many events
    /// are retained after compaction (and how many are loaded on open).
    pub fn open(dir: impl AsRef<Path>, keep: usize) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let keep = keep.max(1);
        fs::create_dir_all(&dir)?;
        let epoch_path = dir.join(EPOCH_FILE);
        if !epoch_path.exists() {
            fs::write(&epoch_path, b"1\n")?;
        }
        let journal_path = dir.join(JOURNAL_FILE);
        if !journal_path.exists() {
            File::create(&journal_path)?;
        }
        Ok(Self {
            dir,
            keep,
            appends_since_compact: 0,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn read_epoch(&self) -> std::io::Result<u64> {
        let raw = fs::read_to_string(self.dir.join(EPOCH_FILE))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(1);
        }
        trimmed
            .parse::<u64>()
            .map(|e| e.max(1))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub fn write_epoch(&self, epoch: u64) -> std::io::Result<()> {
        fs::write(self.dir.join(EPOCH_FILE), format!("{}\n", epoch.max(1)))
    }

    /// Load up to `keep` most recent valid envelopes for the journal's epoch.
    pub fn load_snapshot(&self) -> std::io::Result<JournalSnapshot> {
        let epoch = self.read_epoch()?;
        let path = self.dir.join(JOURNAL_FILE);
        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<WebSocketMessage<Value>>(trimmed) {
                Ok(msg) => {
                    if msg.cursor.map(|c| c.epoch == epoch).unwrap_or(false) {
                        events.push(msg);
                    }
                }
                Err(err) => {
                    warn!(error = %err, "Skipping corrupt event journal line");
                }
            }
        }
        if events.len() > self.keep {
            events = events.split_off(events.len() - self.keep);
        }
        Ok(JournalSnapshot { epoch, events })
    }

    /// Append one stamped envelope. Compacts every `keep` appends.
    pub fn append(&mut self, event: &WebSocketMessage<Value>) -> std::io::Result<()> {
        let path = self.dir.join(JOURNAL_FILE);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        serde_json::to_writer(&mut file, event).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        file.write_all(b"\n")?;
        file.flush()?;
        self.appends_since_compact = self.appends_since_compact.saturating_add(1);
        if self.appends_since_compact >= self.keep {
            self.compact()?;
            self.appends_since_compact = 0;
        }
        Ok(())
    }

    /// Rewrite the journal keeping only the last `keep` same-epoch events.
    pub fn compact(&self) -> std::io::Result<()> {
        let snap = self.load_snapshot()?;
        let tmp = self.dir.join(format!("{JOURNAL_FILE}.tmp"));
        {
            let mut file = File::create(&tmp)?;
            for ev in &snap.events {
                serde_json::to_writer(&mut file, ev)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                file.write_all(b"\n")?;
            }
            file.flush()?;
        }
        fs::rename(&tmp, self.dir.join(JOURNAL_FILE))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::EventCursor;
    use serde_json::json;

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "workmate-event-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn open_persists_epoch_and_roundtrips_events() {
        let dir = tmp_dir();
        let mut journal = EventJournal::open(&dir, 8).unwrap();
        assert_eq!(journal.read_epoch().unwrap(), 1);
        let a = WebSocketMessage::new("a", json!({})).with_cursor(EventCursor::new(1, 1));
        let b = WebSocketMessage::new("b", json!({})).with_cursor(EventCursor::new(1, 2));
        journal.append(&a).unwrap();
        journal.append(&b).unwrap();
        drop(journal);

        let journal = EventJournal::open(&dir, 8).unwrap();
        let snap = journal.load_snapshot().unwrap();
        assert_eq!(snap.epoch, 1);
        assert_eq!(snap.events.len(), 2);
        assert_eq!(snap.events[0].name, "a");
        assert_eq!(snap.events[1].name, "b");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_keeps_last_n() {
        let dir = tmp_dir();
        let mut journal = EventJournal::open(&dir, 2).unwrap();
        for i in 1..=5 {
            let ev = WebSocketMessage::new(format!("e{i}"), json!({})).with_cursor(EventCursor::new(1, i));
            journal.append(&ev).unwrap();
        }
        journal.compact().unwrap();
        let snap = journal.load_snapshot().unwrap();
        assert_eq!(snap.events.len(), 2);
        assert_eq!(snap.events[0].name, "e4");
        assert_eq!(snap.events[1].name, "e5");
        let _ = fs::remove_dir_all(&dir);
    }
}
