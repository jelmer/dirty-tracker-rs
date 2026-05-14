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
///   tracker has missed some events, or if the underlying file system is
///   behaving in an unexpected way.
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
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessError::Timeout(timeout) => write!(f, "Timeout after {:?}", timeout),
            ProcessError::Disconnected => f.write_str("Channel disconnected"),
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
        if self.process_pending(None).is_err() {
            return None;
        }
        if self.need_rescan {
            None
        } else {
            Some(
                self.paths
                    .iter()
                    .map(|p| p.strip_prefix(&self.path).unwrap())
                    .collect(),
            )
        }
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
                    // Insert into paths first to take ownership, then clone for created set
                    self.paths.insert(path.clone());
                    self.created.insert(path);
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
                        self.paths.insert(path);
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
        let dummy = tempfile::NamedTempFile::new_in(&self.path).unwrap();
        let dummy_path = dummy.path().to_path_buf();
        drop(dummy);

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
        let paths = tracker.paths().unwrap();
        if state == State::Unknown {
            panic!("Unexpected unknown state");
        }
        assert_eq!(state, expected_state);
        assert_eq!(paths, expected_paths);
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

    #[test]
    #[allow(deprecated)]
    fn test_is_dirty_deprecated() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        // Test clean state
        assert!(!tracker.is_dirty());

        // Create a file to make it dirty
        let file = dir.path().join("file");
        std::fs::write(&file, b"hello").unwrap();

        wait_for(&mut tracker, &maplit::hashset![file.clone()], State::Dirty);

        // Test dirty state
        assert!(tracker.is_dirty());
    }

    #[test]
    fn test_process_error_display() {
        use std::time::Duration;

        let timeout_error = ProcessError::Timeout(Duration::from_secs(5));
        assert_eq!(timeout_error.to_string(), "Timeout after 5s");

        let disconnected_error = ProcessError::Disconnected;
        assert_eq!(disconnected_error.to_string(), "Channel disconnected");
    }

    #[test]
    fn test_concurrent_modifications() {
        use std::thread;
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        let mut expected = HashSet::new();
        let mut handles = vec![];
        for i in 0..10 {
            let dir_path = dir.path().to_path_buf();
            expected.insert(dir_path.join(format!("concurrent_{}", i)));
            let handle = thread::spawn(move || {
                let file_path = dir_path.join(format!("concurrent_{}", i));
                for j in 0..5 {
                    std::fs::write(&file_path, format!("iteration {}", j)).unwrap();
                    thread::sleep(Duration::from_millis(10));
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        wait_for(&mut tracker, &expected, State::Dirty);
    }

    #[test]
    fn test_rapid_create_delete_cycles() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        let file_path = dir.path().join("rapid_cycle");

        // A file that's been created then deleted is netted out by the
        // tracker's bookkeeping, so the final state is Clean.
        for _ in 0..20 {
            std::fs::write(&file_path, b"test").unwrap();
            std::fs::remove_file(&file_path).unwrap();
        }

        wait_for(&mut tracker, &maplit::hashset![], State::Clean);
    }

    #[test]
    fn test_nested_directory_changes() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        // Build a 5-deep directory structure, then mark clean so that
        // the subsequent file writes are the only events we assert on.
        // Drain events after each mkdir so the recursive watcher attaches
        // watches to each new subdirectory before its child is created
        // (inotify on Linux doesn't auto-recurse).
        let mut current_path = dir.path().to_path_buf();
        for i in 0..5 {
            current_path.push(format!("level_{}", i));
            std::fs::create_dir(&current_path).unwrap();
            let _ = tracker.state();
        }
        tracker.mark_clean();

        let file1 = dir.path().join("level_0").join("file1");
        let file2 = dir.path().join("level_0").join("level_1").join("file2");
        let file3 = current_path.join("deep_file");

        std::fs::write(&file1, b"content1").unwrap();
        std::fs::write(&file2, b"content2").unwrap();
        std::fs::write(&file3, b"content3").unwrap();

        wait_for(
            &mut tracker,
            &maplit::hashset![file1.clone(), file2.clone(), file3.clone()],
            State::Dirty,
        );
    }

    #[test]
    fn test_symlink_handling() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        let target_file = dir.path().join("target");
        let symlink = dir.path().join("symlink");

        std::fs::write(&target_file, b"target content").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_file, &symlink).unwrap();

        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target_file, &symlink).unwrap();

        assert_eq!(tracker.state(), State::Dirty);
        let paths = tracker.paths().unwrap();
        assert!(paths.contains(&target_file));
        assert!(paths.contains(&symlink));
    }

    #[test]
    fn test_file_permissions_change() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempdir().unwrap();
            let mut tracker = DirtyTracker::new(dir.path()).unwrap();

            let file = dir.path().join("perm_test");
            std::fs::write(&file, b"content").unwrap();

            // Clear the state
            tracker.mark_clean();
            assert_eq!(tracker.state(), State::Clean);

            // Change permissions
            let mut perms = std::fs::metadata(&file).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&file, perms).unwrap();

            wait_for(&mut tracker, &maplit::hashset![file.clone()], State::Dirty);
        }
    }

    #[test]
    fn test_large_file_modifications() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        let large_file = dir.path().join("large_file");

        // Create a large file (10MB)
        let large_content = vec![b'a'; 10 * 1024 * 1024];
        std::fs::write(&large_file, &large_content).unwrap();

        assert_eq!(tracker.state(), State::Dirty);
        assert!(tracker.paths().unwrap().contains(&large_file));

        tracker.mark_clean();

        // Modify a small portion of the large file
        use std::io::{Seek, Write};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&large_file)
            .unwrap();
        file.seek(std::io::SeekFrom::Start(5 * 1024 * 1024))
            .unwrap();
        file.write_all(b"modified").unwrap();
        file.sync_all().unwrap();
        drop(file);

        assert_eq!(tracker.state(), State::Dirty);
        assert!(tracker.paths().unwrap().contains(&large_file));
    }

    #[test]
    fn test_many_small_files() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        let mut expected = HashSet::new();
        for batch in 0..5 {
            for i in 0..100 {
                let file = dir.path().join(format!("batch_{}_file_{}", batch, i));
                std::fs::write(&file, format!("content_{}", i)).unwrap();
                expected.insert(file);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        wait_for(&mut tracker, &expected, State::Dirty);
    }

    #[test]
    fn test_many_rapid_writes_to_one_file() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        // 1000 rapid writes to the same file. The event queue absorbs
        // this without overflowing, so the file shows up as the sole
        // dirty path.
        let file = dir.path().join("overflow_test");
        for i in 0..1000 {
            std::fs::write(&file, format!("iteration {}", i)).unwrap();
        }

        wait_for(&mut tracker, &maplit::hashset![file.clone()], State::Dirty);
    }

    #[test]
    fn test_rename_directory_with_contents() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        // Create files in the subdirectory
        for i in 0..5 {
            let file = subdir.join(format!("file_{}", i));
            std::fs::write(&file, format!("content_{}", i)).unwrap();
        }

        tracker.mark_clean();

        // Rename the directory
        let new_subdir = dir.path().join("renamed_subdir");
        std::fs::rename(&subdir, &new_subdir).unwrap();

        assert_eq!(tracker.state(), State::Dirty);
        assert_eq!(
            tracker.paths(),
            Some(&maplit::hashset![subdir.clone(), new_subdir.clone()])
        );
    }

    #[test]
    fn test_process_pending_with_timeout() {
        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();

        // On an idle directory, the sentinel delete event arrives well
        // within the timeout, so process_pending returns Ok.
        let result = tracker.process_pending(Some(std::time::Duration::from_millis(100)));
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }
}
