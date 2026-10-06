//! cedian `Version` ↔ Zed `clock::Global` for one buffer (S9a T3).
//!
//! The headless core keys optimistic edits on `cedian_workspace::Version`, a
//! per-buffer `u64` that bumps on every committed edit. In Zed the truth is
//! the buffer's CRDT version vector. This map gives the headless code its
//! `u64` without a second edit counter: it advances exactly when the
//! buffer's `clock::Global` changes, and resolves a `u64` back to the Global
//! it stood for (the `expected_version` check).

use cedian_workspace::Version;

#[derive(Debug, Default)]
pub struct VersionMap {
    seen: Vec<clock::Global>,
}

impl VersionMap {
    /// The cedian version for the buffer's current `global`.
    pub fn observe(&mut self, global: &clock::Global) -> Version {
        if self.seen.last() != Some(global) {
            self.seen.push(global.clone());
        }
        Version(self.seen.len() as u64 - 1)
    }

    /// The Global a cedian version stood for, if this map issued it.
    pub fn global(&self, version: Version) -> Option<&clock::Global> {
        self.seen.get(usize::try_from(version.0).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use language::Buffer;

    #[gpui::test]
    fn version_advances_exactly_with_the_buffer(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("alpha\nbeta\n", cx));
        let mut map = VersionMap::default();
        let v0 = map.observe(&buffer.read_with(cx, |b, _| b.version()));
        assert_eq!(v0, Version(0));
        // No edit → same version.
        assert_eq!(map.observe(&buffer.read_with(cx, |b, _| b.version())), v0);

        buffer.update(cx, |b, cx| b.edit([(0..5, "ALPHA")], None, cx));
        let g1 = buffer.read_with(cx, |b, _| b.version());
        let v1 = map.observe(&g1);
        assert_eq!(v1, Version(1));
        assert_eq!(map.global(v1), Some(&g1));

        // Undo is a new CRDT state, so a new cedian version (never a rewind).
        buffer.update(cx, |b, cx| {
            b.undo(cx);
        });
        assert_eq!(map.observe(&buffer.read_with(cx, |b, _| b.version())), Version(2));
        assert_ne!(map.global(v0), map.global(Version(2)));
    }
}
