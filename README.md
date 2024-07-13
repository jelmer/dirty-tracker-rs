# Opportunistic dirty file tracker

This library provides a simple way to track dirty files in a directory.
It uses the `notify` crate to watch for file system events and keep track
of the files that have been modified.

If the underlying file system does not support watching for file system events,
or if there are too many files to watch, the tracker will simply give up and
return `State::Unknown`.

Example:

```rust
use dirty_tracker::{State, DirtyTracker};

let td = tempfile::tempdir().unwrap();

let mut tracker = DirtyTracker::new(td.path()).unwrap();
assert_eq!(tracker.state(), State::Clean);
assert!(tracker.paths().unwrap().is_empty());

// Modify a file in the directory.
std::fs::write(td.path().join("file"), b"hello").unwrap();

assert_eq!(tracker.state(), State::Dirty);
assert_eq!(tracker.paths(), Some(&maplit::hashset![td.path().join("file")]));
```
