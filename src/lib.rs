//! Opportunistic dirty file tracker
//!
//! This library provides a simple way to track dirty files in a directory.
//! It uses the `notify` crate to watch for file system events and keep track
//! of the files that have been modified.
//!
//! If the underlying file system does not support watching for file system events, or if there are
//! too many files to watch, the tracker will simply give up and return `State::Unknown`.
//!
//! # Example
//! ```rust
//! use dirty_tracker::{State, DirtyTracker};
//!
//! let td = tempfile::tempdir().unwrap();
//!
//! let mut tracker = DirtyTracker::new(td.path()).unwrap();
//! assert_eq!(tracker.state(), State::Clean);
//! assert!(tracker.paths().unwrap().is_empty());
//!
//! // Modify a file in the directory.
//! std::fs::write(td.path().join("file"), b"hello").unwrap();
//!
//! assert_eq!(tracker.state(), State::Dirty);
//! assert_eq!(tracker.paths(), Some(&maplit::hashset![td.path().join("file")]));
//! ```

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvError, RecvTimeoutError};

/// The tracker object.
///
/// This object keeps track of the dirty files in a directory.
///
/// The tracker is created with a path to a directory. It will watch for file
/// system events in that directory and keep track of the files that have been
/// modified.
///
/// The tracker can be in one of three states:
/// - Clean: No files have been modified.
/// - Dirty: Some files have been modified.
/// - Unknown: The tracker is in an unknown state. This can happen if the
///  tracker has missed some events, or if the underlying file system is
///  behaving in an unexpected way.
pub struct DirtyTracker {
    path: PathBuf,
    rx: Receiver<notify::Result<Event>>,
    paths: HashSet<PathBuf>,
    created: HashSet<PathBuf>,
    need_rescan: bool,
    #[allow(dead_code)]
    watcher: RecommendedWatcher,
}

#[derive(Debug, PartialEq, Eq)]
pub enum State {
    Clean,
    Dirty,
    Unknown,
}

#[derive(Debug)]
pub enum ProcessError {
    Timeout(std::time::Duration),
    Disconnected,
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ProcessError::Timeout(timeout) => write!(f, "Timeout: {:?}", timeout),
            ProcessError::Disconnected => write!(f, "Disconnected"),
        }
    }
}

impl std::error::Error for ProcessError {}

impl DirtyTracker {
    /// Create a new tracker object.
    ///
    /// # Arguments
    /// * `path` - The path to the directory to watch.
    ///
    /// # Returns
    /// A new `DirtyTracker` object.
    pub fn new(path: &Path) -> notify::Result<Self> {
        // Create a channel to receive the events.
        let (tx, rx) = channel();

        let config = notify::Config::default();

        // Create a watcher object.
        let mut watcher: RecommendedWatcher = notify::RecommendedWatcher::new(tx, config)?;

        // TODO: Refuse to work with watchers that are low-performance.

        // Add a path to be watched. All files and directories at that path and below will be monitored for changes.
        watcher.watch(path, RecursiveMode::Recursive)?;

        Ok(DirtyTracker {
            path: path.to_path_buf(),
            rx,
            paths: HashSet::new(),
            created: HashSet::new(),
            need_rescan: false,
            watcher,
        })
    }

    /// Mark all files as clean.
    ///
    /// Note that this can race with file modifications, so it's only safe
    /// if you're sure that no modifications are happening.
    pub fn mark_clean(&mut self) {
        let _ = self.process_pending(None);
        self.need_rescan = false;
        self.paths.clear();
        self.created.clear();
    }

    /// Returns true if there are dirty files.
    #[deprecated(since = "0.2.0", note = "Use state() instead")]
    pub fn is_dirty(&mut self) -> bool {
        self.state() == State::Dirty
    }

    /// Returns the state of the tracker.
    pub fn state(&mut self) -> State {
        if self.process_pending(None).is_err() {
            return State::Unknown;
        }
        if self.need_rescan {
            State::Unknown
        } else if self.paths.is_empty() {
            State::Clean
        } else {
            State::Dirty
        }
    }

    /// Returns the paths of the dirty files.
    ///
    /// If the tracker is in an unknown state, this will return None.
    pub fn paths(&mut self) -> Option<&HashSet<PathBuf>> {
        if self.process_pending(None).is_err() {
            return None;
        }
        if self.need_rescan {
            None
        } else {
            Some(&self.paths)
        }
    }

    /// Returns the relative paths of the dirty files.
    ///
    /// If the tracker is in an unknown state, this will return None.
    pub fn relpaths(&mut self) -> Option<HashSet<&Path>> {
        let path = self.path.clone();
        self.paths().as_mut().map(|paths| {
            paths
                .iter()
                .map(|p| p.strip_prefix(&path).unwrap())
                .collect()
        })
    }

    fn process_pending_event(&mut self, event: Event) {
        if event.need_rescan() {
            self.need_rescan = true;
        }
        match event {
            Event {
                kind: EventKind::Create(_),
                paths,
                ..
            } => {
                for path in paths {
                    self.created.insert(path.clone());
                    self.paths.insert(path);
                }
            }
            Event {
                kind: EventKind::Modify(_),
                paths,
                ..
            } => {
                for path in paths {
                    self.paths.insert(path);
                }
            }
            Event {
                kind: EventKind::Remove(_),
                paths,
                ..
            } => {
                for path in paths {
                    if self.created.contains(&path) {
                        self.paths.remove(&path);
                        self.created.remove(&path);
                    } else {
                        self.paths.insert(path.clone());
                    }
                }
            }
            _ => {}
        }
    }

    fn process_pending(
        &mut self,
        timeout: Option<std::time::Duration>,
    ) -> Result<(), ProcessError> {
        // Make a sentinel change to ensure that we process all pending events.

        // We do this by creating a dummy file and then deleting it
        // immediately.
        //
        // This is a bit of a hack, but it's the simplest way to ensure
        // that we process all pending events.
        //
        // We can't just wait for a timeout, because we might miss events - and it would be
        // difficult to determine the correct timeout value. Performance is one of the main
        // reasons for using this library, so we don't want to wait for a long time.
        let mut dummy = tempfile::NamedTempFile::new_in(&self.path).unwrap();
        use std::io::Write;
        dummy.write_all(b"dummy").unwrap();
        let dummy_path = dummy.path().to_path_buf();
        std::mem::drop(dummy);

        let is_sentinel_delete_event = |event: &notify::Event| {
            matches!(
                event.kind,
                EventKind::Remove(_) if event.paths.iter().any(|p| p == &dummy_path)
            )
        };

        // Process all pending events.
        loop {
            if let Some(timeout) = timeout {
                match self.rx.recv_timeout(timeout) {
                    Ok(Ok(event)) => {
                        if is_sentinel_delete_event(&event) {
                            self.process_pending_event(event);
                            break;
                        } else {
                            self.process_pending_event(event)
                        }
                    }
                    Ok(Err(e)) => {
                        panic!("Error receiving event: {:?}", e);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        return Err(ProcessError::Timeout(timeout));
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(ProcessError::Disconnected);
                    }
                }
            } else {
                match self.rx.recv() {
                    Ok(Ok(event)) => {
                        if is_sentinel_delete_event(&event) {
                            self.process_pending_event(event);
                            break;
                        } else {
                            self.process_pending_event(event)
                        }
                    }
                    Ok(Err(e)) => {
                        panic!("Error receiving event: {:?}", e);
                    }
                    Err(RecvError) => {
                        return Err(ProcessError::Disconnected);
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    fn wait_for(
        tracker: &mut DirtyTracker,
        expected_paths: &HashSet<PathBuf>,
        expected_state: State,
    ) {
        let state = tracker.state();
        let paths = tracker.paths().unwrap().clone();
        if state == State::Unknown {
            panic!("Unexpected unknown state");
        }
        assert_eq!(state, expected_state);
        assert_eq!(paths, *expected_paths);
    }

    #[test]
    fn test_no_changes() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        wait_for(&mut tracker, &maplit::hashset![], State::Clean);

        assert_eq!(tracker.paths(), Some(&maplit::hashset![]));
        assert_eq!(tracker.state(), State::Clean);
    }

    #[test]
    fn test_simple_create() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        let file = dir.path().join("file");
        let mut f = File::create(&file).unwrap();
        f.write_all(b"hello").unwrap();
        f.sync_all().unwrap();
        wait_for(&mut tracker, &maplit::hashset![file.clone()], State::Dirty);
        assert_eq!(tracker.paths(), Some(&maplit::hashset![file.clone()]));
        assert_eq!(
            tracker.relpaths(),
            Some(maplit::hashset![Path::new("file")])
        );
        assert_eq!(tracker.state(), State::Dirty);
    }

    #[test]
    fn test_simple_modify() {
        let dir = tempdir().unwrap();

        let file = dir.path().join("file");
        std::fs::write(&file, b"hello").unwrap();

        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        std::fs::write(&file, b"world").unwrap();

        wait_for(&mut tracker, &maplit::hashset![file.clone()], State::Dirty);
        assert_eq!(tracker.paths(), Some(&maplit::hashset![file.clone()]));
        assert_eq!(
            tracker.relpaths(),
            Some(maplit::hashset![Path::new("file")])
        );
        assert_eq!(tracker.state(), State::Dirty);
    }

    #[test]
    fn test_delete() {
        let dir = tempdir().unwrap();

        let file = dir.path().join("file");
        std::fs::write(&file, b"hello").unwrap();

        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        std::fs::remove_file(&file).unwrap();

        wait_for(&mut tracker, &maplit::hashset![file.clone()], State::Dirty);
        assert_eq!(tracker.paths(), Some(&maplit::hashset![file.clone()]));
        assert_eq!(
            tracker.relpaths(),
            Some(maplit::hashset![Path::new("file")])
        );
        assert_eq!(tracker.state(), State::Dirty);
    }

    #[test]
    fn test_rename() {
        let dir = tempdir().unwrap();

        let file = dir.path().join("file");
        std::fs::write(&file, b"hello").unwrap();

        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        let new_file = dir.path().join("new_file");
        std::fs::rename(&file, &new_file).unwrap();

        wait_for(
            &mut tracker,
            &maplit::hashset![new_file.clone(), file.clone()],
            State::Dirty,
        );

        assert_eq!(
            tracker.paths(),
            Some(&maplit::hashset![file.clone(), new_file.clone()])
        );
        assert_eq!(tracker.state(), State::Dirty);
    }

    #[test]
    fn test_mark_clean() {
        let dir = tempdir().unwrap();

        let file = dir.path().join("file");
        std::fs::write(&file, b"hello").unwrap();

        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        std::fs::write(&file, b"world").unwrap();

        wait_for(&mut tracker, &maplit::hashset![file.clone()], State::Dirty);
        assert_eq!(tracker.paths(), Some(&maplit::hashset![file.clone()]));
        assert_eq!(tracker.state(), State::Dirty);

        tracker.mark_clean();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());
    }

    #[test]
    fn test_add_and_remove() {
        let dir = tempdir().unwrap();

        let file = dir.path().join("file");
        std::fs::write(file, b"hello").unwrap();

        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        let file2 = dir.path().join("file2");
        std::fs::write(&file2, b"world").unwrap();

        wait_for(&mut tracker, &maplit::hashset![file2.clone()], State::Dirty);
        assert_eq!(tracker.paths(), Some(&maplit::hashset![file2.clone()]));
        assert_eq!(tracker.state(), State::Dirty);

        std::fs::remove_file(&file2).unwrap();

        wait_for(&mut tracker, &maplit::hashset![], State::Clean);
        assert_eq!(tracker.paths(), Some(&maplit::hashset![]));
        assert_eq!(tracker.state(), State::Clean);
    }

    #[test]
    fn test_follow_subdir() {
        let dir = tempdir().unwrap();

        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        let file = subdir.join("file");
        std::fs::write(&file, b"hello").unwrap();

        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        std::fs::write(&file, b"world").unwrap();

        wait_for(&mut tracker, &maplit::hashset![file.clone()], State::Dirty);
        assert_eq!(tracker.paths(), Some(&maplit::hashset![file.clone()]));
        assert_eq!(tracker.state(), State::Dirty);
    }

    #[test]
    fn test_create_and_follow_subdir() {
        let dir = tempdir().unwrap();

        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        wait_for(
            &mut tracker,
            &maplit::hashset![subdir.clone()],
            State::Dirty,
        );
        assert_eq!(tracker.paths(), Some(&maplit::hashset![subdir.clone()]));

        let file = subdir.join("file");
        std::fs::write(&file, b"hello").unwrap();

        wait_for(
            &mut tracker,
            &maplit::hashset![subdir.clone(), file.clone()],
            State::Dirty,
        );
        assert_eq!(
            tracker.paths(),
            Some(&maplit::hashset![subdir.clone(), file.clone()])
        );
        assert_eq!(tracker.state(), State::Dirty);
    }

    #[test]
    fn test_many_added() {
        let dir = tempdir().unwrap();

        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert_eq!(tracker.state(), State::Clean);
        assert!(tracker.paths().unwrap().is_empty());

        let mut expected_paths = HashSet::new();

        for i in 0..100 {
            let file = dir.path().join(format!("file{}", i));
            std::fs::write(&file, b"hello").unwrap();
            expected_paths.insert(file.clone());
        }

        wait_for(&mut tracker, &expected_paths, State::Dirty);
        assert_eq!(tracker.paths(), Some(&expected_paths));
        assert_eq!(tracker.state(), State::Dirty);
    }
}
