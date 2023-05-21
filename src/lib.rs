use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct DirtyTracker {
    path: PathBuf,
    inotify: Inotify,
    watch_token: WatchDescriptor,
    paths: HashSet<PathBuf>,
    created: HashSet<PathBuf>,
}

impl DirtyTracker {
    pub fn new(path: &Path) -> std::io::Result<Self> {
        let mut inotify = Inotify::init()?;
        let watch_token = inotify.add_watch(
            path,
            WatchMask::CLOSE_WRITE
                | WatchMask::DELETE
                | WatchMask::MOVED_TO
                | WatchMask::MOVED_FROM
                | WatchMask::ATTRIB,
        )?;

        Ok(DirtyTracker {
            path: path.to_path_buf(),
            inotify,
            watch_token,
            paths: HashSet::new(),
            created: HashSet::new(),
        })
    }

    pub fn mark_clean(&mut self) {
        self.paths.clear();
        self.created.clear();
    }

    pub fn is_dirty(&mut self) -> bool {
        self.process_pending();
        !self.paths.is_empty()
    }

    pub fn paths(&mut self) -> HashSet<PathBuf> {
        self.process_pending();
        self.paths.clone()
    }

    pub fn relpaths(&self) -> HashSet<PathBuf> {
        self.paths
            .iter()
            .map(|p| p.strip_prefix(self.path.as_path()).unwrap().to_path_buf())
            .collect()
    }

    fn process_pending(&mut self) {
        let mut buffer = [0; 1024];
        let events = match self.inotify.read_events(&mut buffer) {
            Ok(events) => events,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) => panic!("Error reading events: {}", error),
        };
        for event in events {
            let path = self.path.join(event.name.unwrap());
            if event.mask.contains(EventMask::CREATE) {
                self.created.insert(path.clone());
            }
            self.paths.insert(path.clone());
            if event.mask.contains(EventMask::DELETE) && self.created.contains(&path) {
                self.paths.remove(&path);
                self.created.remove(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        use super::*;
        use std::fs::File;
        use std::io::Write;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let mut tracker = DirtyTracker::new(dir.path()).unwrap();
        assert!(!tracker.is_dirty());
        assert!(tracker.paths().is_empty());

        let file = dir.path().join("file");
        let mut f = File::create(&file).unwrap();
        f.write_all(b"hello").unwrap();
        assert!(tracker.is_dirty());
        assert_eq!(tracker.paths(), vec![file.clone()].into_iter().collect());

        tracker.mark_clean();
        assert!(!tracker.is_dirty());
        assert!(tracker.paths().is_empty());

        f.write_all(b"world").unwrap();
        f.sync_all().unwrap();
        assert!(tracker.is_dirty());
        assert_eq!(tracker.paths(), vec![file.clone()].into_iter().collect());

        tracker.mark_clean();
        assert!(!tracker.is_dirty());
        assert!(tracker.paths().is_empty());

        let file2 = dir.path().join("file2");
        let mut f2 = File::create(&file2).unwrap();
        f2.write_all(b"hello").unwrap();
        f2.sync_all().unwrap();
        assert!(tracker.is_dirty());
        assert_eq!(
            tracker.paths(),
            vec![file.clone(), file2.clone()].into_iter().collect()
        );

        tracker.mark_clean();
        assert!(!tracker.is_dirty());
        assert!(tracker.paths().is_empty());

        f2.write_all(b"world").unwrap();
        f2.sync_all().unwrap();
        assert!(tracker.is_dirty());
        assert_eq!(
            tracker.paths(),
            vec![file.clone(), file2.clone()].into_iter().collect()
        );

        tracker.mark_clean();
        assert!(!tracker.is_dirty());
        assert!(tracker.paths().is_empty());

        let file3 = dir.path().join("file3");
        let mut f3 = File::create(&file3).unwrap();
        f3.write_all(b"hello").unwrap();
        f3.sync_all().unwrap();
        assert!(tracker.is_dirty());
        assert_eq!(
            tracker.paths(),
            vec![file.clone(), file2.clone(), file3.clone()]
                .into_iter()
                .collect()
        );
    }
}
