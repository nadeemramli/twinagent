//! File watcher over agent data directories.
//!
//! Watches roots like `~/.claude/projects` and `~/.codex/sessions` with
//! platform file events (`notify`), backed by a periodic full rescan — the
//! same belt-and-suspenders approach AgentNotch uses, since inotify/
//! ReadDirectoryChanges can silently drop events and roots may not exist
//! until the first agent session is created.
//!
//! Never point this at `\\wsl$` or `/mnt/c` paths: file events do not cross
//! the WSL/Windows boundary. Each side watches its own native paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// What happened to a data file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    /// File seen for the first time (including files that already existed
    /// when the watcher started — callers resume from a stored offset).
    Created,
    /// File grew (or shrank, after rotation) since we last saw it.
    Grew,
}

/// One change to a watched JSONL file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvent {
    pub path: PathBuf,
    pub kind: FileEventKind,
}

/// Watches directory roots for `.jsonl` changes.
///
/// Sizes are tracked per path, so raw notifications and rescans both funnel
/// through the same dedup: an event is emitted only when a file is new or its
/// size actually changed.
pub struct DirWatcher {
    roots: Vec<PathBuf>,
    /// None if the platform watcher could not be created — rescan-only mode.
    watcher: Option<RecommendedWatcher>,
    rx: Option<Receiver<notify::Result<notify::Event>>>,
    /// Roots we successfully registered with the platform watcher.
    watched_roots: Vec<PathBuf>,
    sizes: HashMap<PathBuf, u64>,
    rescan_interval: Duration,
    last_rescan: Instant,
    /// Events queued by the initial scan / rescans, drained by `poll`.
    pending: Vec<FileEvent>,
}

impl DirWatcher {
    /// Start watching `roots`. Missing roots are fine — they are retried on
    /// every rescan, so a `.codex/sessions` that appears later gets picked
    /// up. All files already present are reported as `Created` on the first
    /// poll.
    pub fn new(roots: Vec<PathBuf>, rescan_interval: Duration) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = notify::recommended_watcher(move |event| {
            // Receiver gone means the DirWatcher was dropped; nothing to do.
            let _ = tx.send(event);
        })
        .ok();

        let mut this = Self {
            roots,
            rx: watcher.as_ref().map(|_| rx),
            watcher,
            watched_roots: Vec::new(),
            sizes: HashMap::new(),
            rescan_interval,
            last_rescan: Instant::now(),
            pending: Vec::new(),
        };
        this.rescan();
        this
    }

    /// Collect events, waiting up to `wait` for the first platform
    /// notification. Runs a full rescan whenever the rescan interval has
    /// elapsed (or on every poll in rescan-only mode).
    pub fn poll(&mut self, wait: Duration) -> Vec<FileEvent> {
        let mut events = std::mem::take(&mut self.pending);

        if let Some(rx) = &self.rx {
            let mut dirty: Vec<PathBuf> = Vec::new();
            // Block briefly for the first notification, then drain the rest.
            match rx.recv_timeout(wait) {
                Ok(Ok(event)) => dirty.extend(event.paths),
                Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.rx = None,
            }
            if let Some(rx) = &self.rx {
                while let Ok(msg) = rx.try_recv() {
                    if let Ok(event) = msg {
                        dirty.extend(event.paths);
                    }
                }
            }
            for path in dirty {
                if is_jsonl(&path) {
                    self.check_file(&path, &mut events);
                }
            }
        } else {
            std::thread::sleep(wait);
        }

        let rescan_due = self.last_rescan.elapsed() >= self.rescan_interval;
        if rescan_due || self.watcher.is_none() {
            self.rescan();
            events.extend(std::mem::take(&mut self.pending));
        }

        events.dedup();
        events
    }

    /// Walk every root, emit events for anything the platform watcher
    /// missed, and (re)register roots that have appeared since the last scan.
    fn rescan(&mut self) {
        self.last_rescan = Instant::now();
        let roots = self.roots.clone();
        for root in &roots {
            if !root.is_dir() {
                continue;
            }
            if let Some(watcher) = &mut self.watcher {
                if !self.watched_roots.contains(root)
                    && watcher.watch(root, RecursiveMode::Recursive).is_ok()
                {
                    self.watched_roots.push(root.clone());
                }
            }
            let mut events = Vec::new();
            walk(root, &mut |path| {
                if is_jsonl(path) {
                    self.check_file(path, &mut events);
                }
            });
            self.pending.extend(events);
        }
    }

    /// Compare a file's current size against what we last saw and record an
    /// event if it changed.
    fn check_file(&mut self, path: &Path, events: &mut Vec<FileEvent>) {
        let Ok(meta) = path.metadata() else {
            // Deleted (session cleanup): forget it so a recreate is Created.
            self.sizes.remove(path);
            return;
        };
        let len = meta.len();
        match self.sizes.insert(path.to_path_buf(), len) {
            None => events.push(FileEvent {
                path: path.to_path_buf(),
                kind: FileEventKind::Created,
            }),
            Some(prev) if prev != len => events.push(FileEvent {
                path: path.to_path_buf(),
                kind: FileEventKind::Grew,
            }),
            Some(_) => {}
        }
    }
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

fn walk(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, visit);
        } else {
            visit(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Poll until `pred` matches the accumulated events or the deadline hits.
    fn poll_until(
        watcher: &mut DirWatcher,
        deadline: Duration,
        pred: impl Fn(&[FileEvent]) -> bool,
    ) -> Vec<FileEvent> {
        let start = Instant::now();
        let mut all = Vec::new();
        while start.elapsed() < deadline {
            all.extend(watcher.poll(Duration::from_millis(50)));
            if pred(&all) {
                break;
            }
        }
        all
    }

    #[test]
    fn initial_scan_reports_existing_files_as_created() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("proj");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("old.jsonl"), b"{}\n").unwrap();
        fs::write(sub.join("ignored.txt"), b"x").unwrap();

        let mut watcher =
            DirWatcher::new(vec![dir.path().to_path_buf()], Duration::from_millis(100));
        let events = watcher.poll(Duration::from_millis(10));
        assert_eq!(
            events,
            vec![FileEvent {
                path: sub.join("old.jsonl"),
                kind: FileEventKind::Created,
            }]
        );
    }

    #[test]
    fn detects_new_and_growing_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher =
            DirWatcher::new(vec![dir.path().to_path_buf()], Duration::from_millis(100));
        watcher.poll(Duration::from_millis(10));

        let file = dir.path().join("s.jsonl");
        fs::write(&file, b"{\"a\":1}\n").unwrap();
        let events = poll_until(&mut watcher, Duration::from_secs(5), |evs| {
            evs.iter()
                .any(|e| e.path == file && e.kind == FileEventKind::Created)
        });
        assert!(
            events
                .iter()
                .any(|e| e.path == file && e.kind == FileEventKind::Created),
            "no Created event: {events:?}"
        );

        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        std::io::Write::write_all(&mut f, b"{\"b\":2}\n").unwrap();
        drop(f);
        let events = poll_until(&mut watcher, Duration::from_secs(5), |evs| {
            evs.iter()
                .any(|e| e.path == file && e.kind == FileEventKind::Grew)
        });
        assert!(
            events
                .iter()
                .any(|e| e.path == file && e.kind == FileEventKind::Grew),
            "no Grew event: {events:?}"
        );
    }

    #[test]
    fn rescan_picks_up_root_created_after_start() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("not-yet");
        let mut watcher = DirWatcher::new(vec![root.clone()], Duration::from_millis(50));
        assert!(watcher.poll(Duration::from_millis(10)).is_empty());

        fs::create_dir(&root).unwrap();
        fs::write(root.join("s.jsonl"), b"{}\n").unwrap();
        let events = poll_until(&mut watcher, Duration::from_secs(5), |evs| !evs.is_empty());
        assert_eq!(events[0].kind, FileEventKind::Created);
        assert_eq!(events[0].path, root.join("s.jsonl"));
    }

    #[test]
    fn unchanged_files_stay_quiet() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("s.jsonl"), b"{}\n").unwrap();
        let mut watcher =
            DirWatcher::new(vec![dir.path().to_path_buf()], Duration::from_millis(20));
        assert_eq!(watcher.poll(Duration::from_millis(10)).len(), 1);

        // Several rescan intervals with no writes: nothing new.
        std::thread::sleep(Duration::from_millis(60));
        assert!(watcher.poll(Duration::from_millis(10)).is_empty());
    }
}
