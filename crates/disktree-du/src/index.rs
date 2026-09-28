//! The persistent index: what the last walk of an operand found, kept so
//! the next one does less.
//!
//! One snapshot per operand, keyed by the directory's device and inode and
//! by the options that shape a walk (`-L`, `-D`, `-x`, the excludes). A
//! snapshot is the walk's whole tree: every listing in `fts` order and every
//! entry's stat.
//!
//! It is used two ways.
//!
//! * Exact, the default. A directory whose inode, ctime and mtime are what
//!   the snapshot recorded has the same entries, because creating, removing
//!   or renaming an entry changes its directory's ctime and mtime. Its
//!   `readdir` is skipped and the recorded names are stat'ed instead. Every
//!   entry is still stat'ed, because a file can grow without its directory
//!   noticing, so the answer is the one a full walk gives.
//!
//!   A timestamp only moves if the change lands in a later tick than the
//!   one it holds. A listing is therefore trusted only when the directory's
//!   ctime was at least [`RACY`] older than the moment the snapshot's walk
//!   began: any change after the listing was read gets a later ctime. This
//!   is git's "racy clean" rule.
//!
//! * Trusted, with `--max-age`. A snapshot younger than the age asked for,
//!   whose operand's own stat has not changed, is the answer: nothing is
//!   walked. Its numbers are as of when it was taken, which `--json` says.
//!
//! The format is private to this version: a snapshot that does not parse
//! exactly is treated as missing.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustc_hash::FxHashMap;
use rustix::io::Errno;

use crate::args::{Deref, Options};
use crate::walk::{
    Entry, ListError, Listing, Meta, Root, Slot, StatError, Time,
};

const MAGIC: &[u8; 8] = b"DTDUIDX1";

/// How much older than the walk a directory's ctime must be before its
/// listing is trusted. Two seconds covers file systems with one-second
/// timestamps and a clock that ticks between the stat and the read.
const RACY: Duration = Duration::from_secs(2);

/// Snapshots kept at most; the least recently used go first.
const KEEP: usize = 256;

/// Where snapshots live, or `None` when there is nowhere sensible.
pub fn directory() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DISKTREE_DU_INDEX_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home.map(|home| home.join("Library/Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| home.map(|home| home.join(".cache")))
    };
    base.map(|base| base.join("disktree").join("du"))
}

/// The options that change what a walk records, folded into a name.
fn fingerprint(options: &Options, root: &Meta) -> u64 {
    // FNV-1a: stable across builds and runs, unlike the std hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(&root.dev.to_le_bytes());
    feed(&root.ino.to_le_bytes());
    feed(&[
        match options.deref {
            Deref::Physical => 0,
            Deref::Args => 1,
            Deref::All => 2,
        },
        u8::from(options.one_file_system),
    ]);
    for pattern in options.excludes.patterns() {
        feed(&(pattern.len() as u64).to_le_bytes());
        feed(pattern);
    }
    hash
}

/// Whether an operand's walk can be kept at all. With `-L` the walk is a
/// graph that can loop, which a tree-shaped snapshot cannot hold.
pub const fn indexable(options: &Options) -> bool {
    options.index && !matches!(options.deref, Deref::All)
}

/// One operand's snapshot, read back.
#[derive(Debug)]
pub struct Snapshot {
    path: PathBuf,
    /// When the walk that produced it began.
    pub taken: Time,
    /// When it was last confirmed by a walk: the file's modification time.
    pub confirmed: SystemTime,
    pub root: Meta,
    pub tree: Arc<Slot>,
}

impl Snapshot {
    /// The file a snapshot of `root` under `options` lives in.
    pub fn file(dir: &Path, options: &Options, root: &Meta) -> PathBuf {
        dir.join(format!("{:016x}.idx", fingerprint(options, root)))
    }

    pub fn load(path: PathBuf) -> Option<Self> {
        let bytes = fs::read(&path).ok()?;
        let confirmed = fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        let mut reader = Reader {
            bytes: &bytes,
            at: 0,
        };
        if reader.take(MAGIC.len())? != MAGIC {
            return None;
        }
        let taken = reader.time()?;
        let root = reader.meta()?;
        let tree = Arc::new(Slot::new());
        let _ = tree.set(reader.listing()?);
        if reader.at != bytes.len() {
            return None;
        }
        Some(Self {
            path,
            taken,
            confirmed,
            root,
            tree,
        })
    }

    /// Whether this snapshot may stand in for a walk: young enough, and
    /// the operand itself unchanged since.
    pub fn trusted(&self, max_age: Duration, root: &Meta) -> bool {
        let fresh = SystemTime::now()
            .duration_since(self.confirmed)
            .is_ok_and(|age| age <= max_age);
        fresh && self.root.unchanged(root)
    }

    /// Mark the snapshot as confirmed now, without rewriting it.
    pub fn confirm(&self) {
        if let Ok(file) = fs::File::options().append(true).open(&self.path) {
            let _ = file.set_modified(SystemTime::now());
        }
    }

    /// The listings a walk may reuse, by directory identity.
    pub fn previous(&self) -> Previous {
        let mut dirs = FxHashMap::default();
        let taken = self.taken;
        let limit = Time {
            sec: taken.sec.saturating_sub(RACY.as_secs().cast_signed()),
            nsec: taken.nsec,
        };
        let mut stack: Vec<(Meta, Arc<Slot>)> =
            vec![(self.root, Arc::clone(&self.tree))];
        while let Some((meta, slot)) = stack.pop() {
            let Some(listing) = slot.get() else { continue };
            if listing.error.is_none()
                && meta.ctime < limit
                && meta.mtime < limit
            {
                dirs.insert(meta.key(), (meta, Arc::clone(&slot)));
            }
            for entry in &listing.entries {
                if let (Ok(child), Some(dir)) = (entry.meta, &entry.dir) {
                    stack.push((child, Arc::clone(dir)));
                }
            }
        }
        Previous { dirs }
    }
}

/// The listings the exact mode may reuse.
#[derive(Debug, Default)]
pub struct Previous {
    dirs: FxHashMap<(u64, u64), (Meta, Arc<Slot>)>,
}

impl Previous {
    /// The recorded names of `dir`, in order, when it provably has not
    /// changed since they were read.
    pub fn names(&self, dir: Meta) -> Option<Vec<(Box<[u8]>, u64)>> {
        let (then, slot) = self.dirs.get(&dir.key())?;
        if then.ctime != dir.ctime || then.mtime != dir.mtime {
            return None;
        }
        let listing = slot.get()?;
        Some(
            listing
                .entries
                .iter()
                .map(|entry| (entry.name.clone(), entry.d_ino))
                .collect(),
        )
    }
}

/// Whether a walk found exactly what `snapshot` holds, in which case the
/// snapshot only needs to be marked as confirmed.
pub fn same(snapshot: &Snapshot, root: &Root) -> bool {
    fn listings(a: &Listing, b: &Listing) -> bool {
        a.error == b.error
            && a.entries.len() == b.entries.len()
            && a.entries.iter().zip(&b.entries).all(|(a, b)| {
                a.name == b.name
                    && a.d_ino == b.d_ino
                    && match (&a.meta, &b.meta) {
                        (Ok(a), Ok(b)) => a.unchanged(b),
                        (a, b) => a == b,
                    }
                    && match (
                        a.dir.as_deref().and_then(OnceLock::get),
                        b.dir.as_deref().and_then(OnceLock::get),
                    ) {
                        (None, None) => true,
                        (Some(a), Some(b)) => listings(a, b),
                        _ => false,
                    }
            })
    }
    root.meta.is_ok_and(|meta| meta.unchanged(&snapshot.root))
        && match (
            snapshot.tree.get(),
            root.dir.as_deref().and_then(OnceLock::get),
        ) {
            (Some(a), Some(b)) => listings(a, b),
            _ => false,
        }
}

/// Write `root`'s walk as a snapshot, replacing the file atomically.
pub fn save(
    dir: &Path,
    options: &Options,
    root: &Root,
    taken: Time,
) -> std::io::Result<()> {
    let (Ok(meta), Some(slot)) = (root.meta, &root.dir) else {
        return Ok(());
    };
    let Some(listing) = slot.get() else {
        return Ok(());
    };
    create_private_dir(dir)?;
    let mut out = Vec::with_capacity(1 << 16);
    out.extend_from_slice(MAGIC);
    put_time(&mut out, taken);
    put_meta(&mut out, &meta);
    put_listing(&mut out, listing);
    let path = Snapshot::file(dir, options, &meta);
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    let result = (|| {
        let mut file = private_file(&temp)?;
        file.write_all(&out)?;
        file.sync_data()?;
        fs::rename(&temp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Drop the least recently confirmed snapshots beyond [`KEEP`].
pub fn prune(dir: &Path) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, PathBuf)> = read
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "idx")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    if files.len() <= KEEP {
        return;
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    for (_, path) in files.into_iter().skip(KEEP) {
        let _ = fs::remove_file(path);
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    // File names are private: the index is readable by its owner only.
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

fn private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

pub fn now() -> Time {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Time {
        sec: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        nsec: since.subsec_nanos(),
    }
}

pub fn system_time(time: Time) -> SystemTime {
    let secs = u64::try_from(time.sec).unwrap_or(0);
    UNIX_EPOCH + Duration::new(secs, time.nsec)
}

// The encoding: LEB128 varints, zigzag for signed values, and a listing as
// its error, its entry count, then each entry followed by its own listing
// when it has one.

fn put_u64(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    put_u64(out, ((value << 1) ^ (value >> 63)).cast_unsigned());
}

fn put_time(out: &mut Vec<u8>, time: Time) {
    put_i64(out, time.sec);
    put_u64(out, u64::from(time.nsec));
}

fn put_meta(out: &mut Vec<u8>, meta: &Meta) {
    put_u64(out, meta.dev);
    put_u64(out, meta.ino);
    put_u64(out, u64::from(meta.mode));
    put_u64(out, meta.nlink);
    put_u64(out, meta.blocks);
    put_i64(out, meta.size);
    put_time(out, meta.mtime);
    put_time(out, meta.atime);
    put_time(out, meta.ctime);
}

fn put_errno(out: &mut Vec<u8>, errno: Errno) {
    put_u64(out, u64::from(errno.raw_os_error().unsigned_abs()));
}

fn put_listing(out: &mut Vec<u8>, listing: &Listing) {
    match listing.error {
        None => put_u64(out, 0),
        Some(ListError::Unreadable(errno)) => {
            put_u64(out, 1);
            put_errno(out, errno);
        }
        Some(ListError::Partial(errno)) => {
            put_u64(out, 2);
            put_errno(out, errno);
        }
    }
    put_u64(out, listing.entries.len() as u64);
    for entry in &listing.entries {
        put_u64(out, entry.name.len() as u64);
        out.extend_from_slice(&entry.name);
        put_u64(out, entry.d_ino);
        match entry.meta {
            Ok(meta) => {
                out.push(0);
                put_meta(out, &meta);
            }
            Err(StatError::Errno(errno)) => {
                out.push(1);
                put_errno(out, errno);
            }
            Err(StatError::Dangling) => out.push(2),
        }
        match entry.dir.as_deref().and_then(OnceLock::get) {
            Some(child) => {
                out.push(1);
                put_listing(out, child);
            }
            None => out.push(0),
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(len)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u64(&mut self) -> Option<u64> {
        let mut value: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn i64(&mut self) -> Option<i64> {
        let raw = self.u64()?;
        Some((raw >> 1).cast_signed() ^ -(raw & 1).cast_signed())
    }

    fn time(&mut self) -> Option<Time> {
        Some(Time {
            sec: self.i64()?,
            nsec: u32::try_from(self.u64()?).ok()?,
        })
    }

    fn meta(&mut self) -> Option<Meta> {
        Some(Meta {
            dev: self.u64()?,
            ino: self.u64()?,
            mode: u32::try_from(self.u64()?).ok()?,
            nlink: self.u64()?,
            blocks: self.u64()?,
            size: self.i64()?,
            mtime: self.time()?,
            atime: self.time()?,
            ctime: self.time()?,
        })
    }

    fn errno(&mut self) -> Option<Errno> {
        Some(Errno::from_raw_os_error(i32::try_from(self.u64()?).ok()?))
    }

    fn listing(&mut self) -> Option<Listing> {
        let error = match self.u64()? {
            0 => None,
            1 => Some(ListError::Unreadable(self.errno()?)),
            2 => Some(ListError::Partial(self.errno()?)),
            _ => return None,
        };
        let count = usize::try_from(self.u64()?).ok()?;
        // A corrupt count must not become a huge allocation.
        let mut entries = Vec::with_capacity(count.min(self.bytes.len()));
        for _ in 0..count {
            let len = usize::try_from(self.u64()?).ok()?;
            let name: Box<[u8]> = self.take(len)?.into();
            let d_ino = self.u64()?;
            let meta = match self.byte()? {
                0 => Ok(self.meta()?),
                1 => Err(StatError::Errno(self.errno()?)),
                2 => Err(StatError::Dangling),
                _ => return None,
            };
            let dir = match self.byte()? {
                0 => None,
                1 => {
                    let slot = Arc::new(Slot::new());
                    let _ = slot.set(self.listing()?);
                    Some(slot)
                }
                _ => return None,
            };
            entries.push(Entry {
                name,
                d_ino,
                meta,
                dir,
            });
        }
        Some(Listing {
            entries,
            error,
            reused: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(ino: u64, sec: i64) -> Meta {
        let time = Time { sec, nsec: 5 };
        Meta {
            dev: 7,
            ino,
            mode: 0o040_755,
            nlink: 2,
            blocks: 8,
            size: -1,
            mtime: time,
            atime: time,
            ctime: time,
        }
    }

    #[test]
    fn varints_round_trip() {
        let mut out = Vec::new();
        for value in [0, 1, 127, 128, u64::MAX] {
            put_u64(&mut out, value);
        }
        for value in [0, -1, 1, i64::MIN, i64::MAX] {
            put_i64(&mut out, value);
        }
        let mut reader = Reader { bytes: &out, at: 0 };
        for value in [0, 1, 127, 128, u64::MAX] {
            assert_eq!(reader.u64(), Some(value));
        }
        for value in [0, -1, 1, i64::MIN, i64::MAX] {
            assert_eq!(reader.i64(), Some(value));
        }
    }

    #[test]
    fn listings_round_trip_and_racy_ones_are_not_reused() {
        let child = Listing {
            entries: vec![Entry {
                name: b"f".as_slice().into(),
                d_ino: 3,
                meta: Err(StatError::Errno(Errno::ACCESS)),
                dir: None,
            }],
            ..Listing::default()
        };
        let slot = Arc::new(Slot::new());
        let _ = slot.set(child);
        let listing = Listing {
            entries: vec![
                Entry {
                    name: b"old".as_slice().into(),
                    d_ino: 2,
                    meta: Ok(meta(2, 100)),
                    dir: Some(slot),
                },
                Entry {
                    name: b"recent".as_slice().into(),
                    d_ino: 9,
                    meta: Ok(meta(9, 999)),
                    dir: Some(Arc::new(Slot::new())),
                },
            ],
            ..Listing::default()
        };
        let mut out = Vec::new();
        put_listing(&mut out, &listing);
        let mut reader = Reader { bytes: &out, at: 0 };
        let back = reader.listing().expect("parses");
        assert_eq!(reader.at, out.len());
        assert_eq!(back.entries.len(), 2);

        let tree = Arc::new(Slot::new());
        let _ = tree.set(back);
        let snapshot = Snapshot {
            path: PathBuf::new(),
            taken: Time { sec: 1000, nsec: 0 },
            confirmed: SystemTime::now(),
            root: meta(1, 10),
            tree,
        };
        let previous = snapshot.previous();
        assert_eq!(
            previous.names(meta(2, 100)).map(|names| names.len()),
            Some(1),
            "an old, unchanged directory is reused"
        );
        assert!(
            previous.names(meta(2, 101)).is_none(),
            "a changed one is read again"
        );
        assert!(
            previous.names(meta(9, 999)).is_none(),
            "one changed within the racy window is never trusted"
        );
    }
}
