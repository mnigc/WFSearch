//! Compact per-volume filename index.
//!
//! One `VolumeIndex` per NTFS volume. Nodes live in a flat `Vec` addressed by
//! slot; file names live packed in a byte arena. Tree structure is kept via
//! parent slot references, so path strings are only materialized for query
//! results. Deletes are tombstones (subtree BFS-marked) and the memory is
//! reclaimed by a full rebuild once a volume accumulates too many.

pub const FLAG_DIR: u16 = 1;
pub const FLAG_DELETED: u16 = 2;
/// NTFS root directory File Reference Number.
pub const ROOT_FRN: u64 = 5;

const MAX_PATH_DEPTH: usize = 512;

/// Packed UTF-8 name arena. Invariant: only valid UTF-8 ever enters `buf`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NamePool {
    pub buf: Vec<u8>,
}

impl NamePool {
    pub fn push(&mut self, name: &str) -> (u32, u32) {
        let off = self.buf.len() as u32;
        self.buf.extend_from_slice(name.as_bytes());
        (off, name.len() as u32)
    }

    #[inline]
    pub fn get(&self, off: u32, len: u32) -> &[u8] {
        &self.buf[off as usize..(off + len) as usize]
    }

    #[inline]
    pub fn get_str(&self, off: u32, len: u32) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.get(off, len)) }
    }
}

/// ~24 bytes per file/dir.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Node {
    pub frn: u64,
    /// slot of parent directory; `u32::MAX` for the volume root
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u32,
    pub flags: u16,
    #[serde(skip)]
    pub _pad: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Added,
    Updated,
    /// parent not present (or deleted) yet
    Orphan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameOutcome {
    Updated,
    Missing,
    Orphan,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VolumeIndex {
    pub drive: char,
    pub names: NamePool,
    pub nodes: Vec<Node>,
    /// FRN -> slot (root included)
    pub frn_index: ahash::AHashMap<u64, u32>,
    /// parent slot -> child slots (dirs only)
    pub children: ahash::AHashMap<u32, Vec<u32>>,
    /// tombstoned nodes (reclaimed on rebuild)
    pub deleted: u32,
}

impl VolumeIndex {
    pub fn new(drive: char) -> VolumeIndex {
        let mut v = VolumeIndex {
            drive,
            names: NamePool::default(),
            nodes: Vec::new(),
            frn_index: ahash::AHashMap::default(),
            children: ahash::AHashMap::default(),
            deleted: 0,
        };
        let root_name = format!("{drive}:");
        let (off, len) = v.names.push(&root_name);
        v.nodes.push(Node {
            frn: ROOT_FRN,
            parent: u32::MAX,
            name_off: off,
            name_len: len,
            flags: FLAG_DIR,
            _pad: 0,
        });
        v.frn_index.insert(ROOT_FRN, 0);
        v
    }

    #[inline]
    pub fn live_count(&self) -> u64 {
        self.nodes.len() as u64 - self.deleted as u64
    }

    #[inline]
    pub fn is_live(&self, slot: u32) -> bool {
        self.nodes[slot as usize].flags & FLAG_DELETED == 0
    }

    fn slot_of(&self, frn: u64) -> Option<u32> {
        self.frn_index.get(&frn).copied()
    }

    /// Slot of a live (non-tombstoned) node.
    fn live_slot_of(&self, frn: u64) -> Option<u32> {
        let slot = self.slot_of(frn)?;
        if self.nodes[slot as usize].flags & FLAG_DELETED != 0 {
            None
        } else {
            Some(slot)
        }
    }

    /// Insert a new entry or update an existing one (same FRN seen again —
    /// USN journals can re-emit, and FRNs get reused after deletion).
    pub fn insert(&mut self, frn: u64, parent_frn: u64, name: &str, is_dir: bool) -> InsertOutcome {
        if frn == ROOT_FRN {
            return InsertOutcome::Updated;
        }
        if let Some(&slot) = self.frn_index.get(&frn) {
            let Some(pslot) = self.live_slot_of(parent_frn) else {
                return InsertOutcome::Orphan;
            };
            let old_parent = self.nodes[slot as usize].parent;
            if old_parent != pslot {
                if let Some(list) = self.children.get_mut(&old_parent) {
                    list.retain(|&c| c != slot);
                }
                self.children.entry(pslot).or_default().push(slot);
                self.nodes[slot as usize].parent = pslot;
            }
            let was_deleted = self.nodes[slot as usize].flags & FLAG_DELETED != 0;
            let (off, len) = self.names.push(name);
            let n = &mut self.nodes[slot as usize];
            n.name_off = off;
            n.name_len = len;
            n.flags = if is_dir {
                n.flags | FLAG_DIR
            } else {
                n.flags & !FLAG_DIR
            } & !FLAG_DELETED;
            if was_deleted {
                self.deleted -= 1;
            }
            InsertOutcome::Updated
        } else {
            let Some(pslot) = self.live_slot_of(parent_frn) else {
                return InsertOutcome::Orphan;
            };
            let (off, len) = self.names.push(name);
            let slot = self.nodes.len() as u32;
            self.nodes.push(Node {
                frn,
                parent: pslot,
                name_off: off,
                name_len: len,
                flags: if is_dir { FLAG_DIR } else { 0 },
                _pad: 0,
            });
            self.frn_index.insert(frn, slot);
            self.children.entry(pslot).or_default().push(slot);
            InsertOutcome::Added
        }
    }

    /// Tombstone an entry; if it is a directory, tombstone the whole subtree
    /// (children are reachable via the children map). Ghost descendants of a
    /// tombstoned subtree stay tombstoned even if the dir FRN is later reused.
    pub fn mark_deleted(&mut self, frn: u64) -> bool {
        let Some(slot) = self.frn_index.remove(&frn) else {
            return false;
        };
        let mut stack = vec![slot];
        while let Some(s) = stack.pop() {
            {
                let n = &mut self.nodes[s as usize];
                if n.flags & FLAG_DELETED != 0 {
                    continue;
                }
                n.flags |= FLAG_DELETED;
                self.deleted += 1;
            }
            if let Some(kids) = self.children.remove(&s) {
                for k in kids {
                    let kf = self.nodes[k as usize].frn;
                    self.frn_index.remove(&kf);
                    stack.push(k);
                }
            }
        }
        true
    }

    /// Move/rename an entry in place (keeps the slot, so children of a
    /// renamed directory stay attached).
    pub fn rename(&mut self, frn: u64, parent_frn: u64, name: &str) -> RenameOutcome {
        let Some(slot) = self.slot_of(frn) else {
            return RenameOutcome::Missing;
        };
        if self.nodes[slot as usize].flags & FLAG_DELETED != 0 {
            return RenameOutcome::Missing;
        }
        let Some(pslot) = self.live_slot_of(parent_frn) else {
            return RenameOutcome::Orphan;
        };
        let old_parent = self.nodes[slot as usize].parent;
        if old_parent != pslot {
            if let Some(list) = self.children.get_mut(&old_parent) {
                list.retain(|&c| c != slot);
            }
            self.children.entry(pslot).or_default().push(slot);
            self.nodes[slot as usize].parent = pslot;
        }
        let (off, len) = self.names.push(name);
        let n = &mut self.nodes[slot as usize];
        n.name_off = off;
        n.name_len = len;
        RenameOutcome::Updated
    }

    /// Full path like `C:\work\a.txt`. Root itself renders as `C:\`.
    pub fn path_of(&self, slot: u32) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = slot;
        let mut depth = 0;
        while cur != 0 && cur != u32::MAX && depth < MAX_PATH_DEPTH {
            let n = &self.nodes[cur as usize];
            parts.push(self.names.get_str(n.name_off, n.name_len));
            cur = n.parent;
            depth += 1;
        }
        let root = &self.nodes[0];
        parts.push(self.names.get_str(root.name_off, root.name_len));
        let cap: usize = parts.iter().map(|p| p.len() + 1).sum();
        let mut s = String::with_capacity(cap);
        let mut first = true;
        for p in parts.iter().rev() {
            if !first {
                s.push('\\');
            }
            s.push_str(p);
            first = false;
        }
        if slot == 0 {
            s.push('\\');
        }
        s
    }

    pub fn approx_memory(&self) -> u64 {
        let dirs = self.children.len() as u64;
        self.names.buf.len() as u64
            + self.nodes.len() as u64 * std::mem::size_of::<Node>() as u64
            + self.frn_index.len() as u64 * 24
            + dirs * 64
            + self.nodes.len() as u64 * 4
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> VolumeIndex {
        VolumeIndex::new('C')
    }

    /// build a small tree:
    /// C:\
    /// ├── work\            (10)
    /// │   ├── a.txt        (11)
    /// │   └── sub\         (12)
    /// │       └── b.md     (13)
    /// └── single.log       (14)
    fn tree() -> VolumeIndex {
        let mut v = idx();
        assert_eq!(v.insert(10, 5, "work", true), InsertOutcome::Added);
        assert_eq!(v.insert(11, 10, "a.txt", false), InsertOutcome::Added);
        assert_eq!(v.insert(12, 10, "sub", true), InsertOutcome::Added);
        assert_eq!(v.insert(13, 12, "b.md", false), InsertOutcome::Added);
        assert_eq!(v.insert(14, 5, "single.log", false), InsertOutcome::Added);
        v
    }

    #[test]
    fn insert_and_paths() {
        let v = tree();
        assert_eq!(v.live_count(), 6); // root + 5 entries
        let s11 = v.slot_of(11).unwrap();
        assert_eq!(v.path_of(s11), "C:\\work\\a.txt");
        let s12 = v.slot_of(12).unwrap();
        assert_eq!(v.path_of(s12), "C:\\work\\sub");
        let s0 = v.slot_of(5).unwrap();
        assert_eq!(v.path_of(s0), "C:\\");
    }

    #[test]
    fn orphan_then_parent() {
        let mut v = idx();
        assert_eq!(v.insert(100, 99, "kid.txt", false), InsertOutcome::Orphan);
        assert_eq!(v.live_count(), 1); // only root
        assert_eq!(v.insert(99, 5, "dad", true), InsertOutcome::Added);
        assert_eq!(v.insert(100, 99, "kid.txt", false), InsertOutcome::Added);
        let s = v.slot_of(100).unwrap();
        assert_eq!(v.path_of(s), "C:\\dad\\kid.txt");
    }

    #[test]
    fn delete_dir_removes_subtree() {
        let mut v = tree();
        assert!(v.mark_deleted(10)); // work\
        assert_eq!(v.live_count(), 2); // root + single.log
        assert!(v.slot_of(11).is_none());
        assert!(v.slot_of(13).is_none());
        assert!(!v.mark_deleted(10)); // already gone from frn_index
        assert!(v.mark_deleted(14));
        assert_eq!(v.live_count(), 1);
        // root cannot be deleted via mark_deleted(5)? frn 5 is in frn_index — deleting root is not expected; document-only.
    }

    #[test]
    fn rename_file_and_dir() {
        let mut v = tree();
        assert_eq!(v.rename(11, 10, "renamed.txt"), RenameOutcome::Updated);
        let s = v.slot_of(11).unwrap();
        assert_eq!(v.path_of(s), "C:\\work\\renamed.txt");
        // rename dir first — children must follow the renamed parent
        assert_eq!(v.rename(12, 10, "sub2"), RenameOutcome::Updated);
        let s13 = v.slot_of(13).unwrap();
        assert_eq!(v.path_of(s13), "C:\\work\\sub2\\b.md");
        // then move b.md from sub2\ to work\
        assert_eq!(v.rename(13, 10, "b.md"), RenameOutcome::Updated);
        let s13b = v.slot_of(13).unwrap();
        assert_eq!(v.path_of(s13b), "C:\\work\\b.md");
        assert_eq!(v.rename(999, 10, "x"), RenameOutcome::Missing);
        assert_eq!(v.rename(11, 777, "x"), RenameOutcome::Orphan);
    }

    #[test]
    fn frn_reuse_resurrects() {
        let mut v = tree();
        v.mark_deleted(11);
        assert_eq!(v.live_count(), 5); // 6 - a.txt
                                       // same FRN recreated with a new name under root; the old tombstoned
                                       // slot is unreachable (frn_index entry was dropped), so this is Added
        assert_eq!(v.insert(11, 5, "new.txt", false), InsertOutcome::Added);
        assert_eq!(v.live_count(), 6);
        let s = v.slot_of(11).unwrap();
        assert_eq!(v.path_of(s), "C:\\new.txt");
        assert!(v.is_live(s));
    }

    #[test]
    fn insert_under_deleted_parent_is_orphan() {
        let mut v = tree();
        v.mark_deleted(12); // sub\ tombstoned (b.md tombstoned too)
        assert_eq!(
            v.insert(20, 12, "under-ghost.txt", false),
            InsertOutcome::Orphan
        );
    }
}
