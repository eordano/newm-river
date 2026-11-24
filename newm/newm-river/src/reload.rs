//! Config hot reload, as niri's `src/utils/watcher.rs` does it: the config file is looked at
//! every 500 ms, and a change of what it resolves to (a symlink swapped, as NixOS swaps /etc), of
//! its modification time or of its size reloads it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const PERIOD: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    target: PathBuf,
    modified: SystemTime,
    len: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let target = fs::canonicalize(path).ok()?;
    let meta = fs::metadata(&target).ok()?;
    Some(Stamp {
        modified: meta.modified().ok()?,
        len: meta.len(),
        target,
    })
}

pub struct Watcher {
    path: PathBuf,
    stamp: Option<Stamp>,
    next: Instant,
}

impl Watcher {
    pub fn new(path: &Path, now: Instant) -> Self {
        Self {
            path: path.to_path_buf(),
            stamp: stamp(path),
            next: now + PERIOD,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// When the file is due to be looked at again.
    pub fn deadline(&self) -> Instant {
        self.next
    }

    /// Whether the file changed since it was last looked at; a file gone changes nothing until
    /// it comes back.
    pub fn poll(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + PERIOD;
        match stamp(&self.path) {
            Some(s) if self.stamp.as_ref() != Some(&s) => {
                self.stamp = Some(s);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_or_a_symlink_swap_is_a_change_and_nothing_else_is() {
        let dir = std::env::temp_dir().join(format!("newm-river-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (a, b, link) = (dir.join("a.kdl"), dir.join("b.kdl"), dir.join("config.kdl"));
        fs::write(&a, "layout {}\n").unwrap();
        fs::write(&b, "layout {}\n").unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let now = Instant::now();
        let mut w = Watcher::new(&link, now);
        assert!(!w.poll(now), "not due yet");
        let mut t = now + PERIOD;
        assert!(!w.poll(t), "unchanged");
        fs::write(&a, "layout { gaps 4; }\n").unwrap();
        t += PERIOD;
        assert!(w.poll(t), "an edit of the target");
        fs::remove_file(&link).unwrap();
        t += PERIOD;
        assert!(!w.poll(t), "gone: nothing to load");
        std::os::unix::fs::symlink(&b, &link).unwrap();
        t += PERIOD;
        assert!(w.poll(t), "the link swapped to another file");
        t += PERIOD;
        assert!(!w.poll(t));
        fs::remove_dir_all(&dir).unwrap();
    }
}
