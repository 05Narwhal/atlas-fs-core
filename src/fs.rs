// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! The `FileSystem` trait and its data types.
//!
//! A filesystem is a tree of nodes named by [`NodeId`]. Folders are listed
//! through a [`DirStream`], which fills [`DirBatch`]es: one allocation for
//! all the names of a page instead of one per entry.
//!
//! The trait holds what has an implementation and a caller. Methods a
//! format may lack have defaults (`Unsupported` or empty), so a new format
//! starts with the five required methods and grows from there.

use std::collections::{HashSet, VecDeque};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{CorruptKind, Corruption, Error, Location, Result};
use crate::io::block::BlockSource;

/// Names a file or folder within one [`FileSystem`]: the CNID on HFS, the
/// inode number on APFS, an interned path on the host filesystem.
///
/// An id is only meaningful to the filesystem that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

/// What a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// A folder.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link. The link itself, never its target.
    Symlink,
    /// Anything else: device nodes, sockets, pipes.
    Other,
}

/// Facts about an entry that are cheap to learn while listing its folder.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EntryFlags(u16);

impl EntryFlags {
    /// No flags.
    pub const EMPTY: Self = Self(0);
    /// The filesystem marks the entry as hidden.
    pub const HIDDEN: Self = Self(1);
    /// The entry is a symbolic link whose target is a folder.
    pub const LINKS_TO_DIRECTORY: Self = Self(1 << 1);
    /// The file's content is stored compressed by the filesystem (HFS+ and
    /// APFS decmpfs). Sizes are the uncompressed size.
    pub const COMPRESSED: Self = Self(1 << 2);
    /// The Finder's bundle bit (`kHasBundle`) is set. On a folder it marks a
    /// package (shown as one item); on a classic file it means the file
    /// carries a `BNDL` resource, usually an application. Raw: whoever asks
    /// "is this a package?" also checks that the entry is a folder.
    pub const PACKAGE: Self = Self(1 << 3);
    /// The Finder's alias bit (`kIsAlias`) is set: the file is a classic
    /// alias whose target is in its `alis` resource.
    pub const ALIAS: Self = Self(1 << 4);

    /// Shift of the Finder label (colour) bits.
    const LABEL_SHIFT: u32 = 8;
    /// Mask of the Finder label bits, 8 to 10.
    const LABEL_MASK: u16 = 0b111 << Self::LABEL_SHIFT;

    /// True when every flag in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The flags of `self` and `other` together.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// True when no flag is set and there is no label.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The Finder label, 0 (none) to 7: the Finder's three colour bits as
    /// stored. Mac OS X order: 1 Gray, 2 Green, 3 Purple, 4 Blue, 5 Yellow,
    /// 6 Red, 7 Orange.
    pub const fn label(self) -> u8 {
        // Three bits: always fits.
        ((self.0 & Self::LABEL_MASK) >> Self::LABEL_SHIFT) as u8
    }

    /// `self` with its label replaced by the low three bits of `label`.
    #[must_use]
    pub const fn with_label(self, label: u8) -> Self {
        let bits = ((label as u16) << Self::LABEL_SHIFT) & Self::LABEL_MASK;
        Self((self.0 & !Self::LABEL_MASK) | bits)
    }
}

impl std::fmt::Debug for EntryFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EntryFlags({:#06b})", self.0)
    }
}

const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// A point in time: nanoseconds since the Unix epoch, UTC.
///
/// Covers the years 1677 to 2262. Constructors saturate instead of failing,
/// because a date field on a damaged disk can hold anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// From nanoseconds since the Unix epoch.
    pub const fn from_unix_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// From seconds since the Unix epoch plus a nanosecond part. Saturates.
    pub const fn from_unix(seconds: i64, nanos: u32) -> Self {
        // `as` is lossless here: u32 always fits i64, and `From` is not const.
        Self(
            seconds
                .saturating_mul(NANOS_PER_SECOND)
                .saturating_add(nanos as i64),
        )
    }

    /// From a host clock value. Saturates.
    pub fn from_system_time(time: SystemTime) -> Self {
        fn nanos(duration: Duration) -> i64 {
            i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
        }
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self(nanos(after)),
            Err(before) => Self(nanos(before.duration()).saturating_neg()),
        }
    }

    /// Nanoseconds since the Unix epoch.
    pub const fn unix_nanos(self) -> i64 {
        self.0
    }

    /// Whole seconds since the Unix epoch, rounded toward the past.
    pub const fn unix_seconds(self) -> i64 {
        self.0.div_euclid(NANOS_PER_SECOND)
    }

    /// The nanosecond part that [`Self::unix_seconds`] leaves out, 0 to 999,999,999.
    pub const fn subsec_nanos(self) -> u32 {
        // rem_euclid by a positive constant is in 0..1e9, which fits u32.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.0.rem_euclid(NANOS_PER_SECOND) as u32
        }
    }
}

/// One entry of a folder, borrowed from the [`DirBatch`] that holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry<'a> {
    /// The entry's node.
    pub node: NodeId,
    /// The entry's name, without any path.
    pub name: &'a str,
    /// What the entry is.
    pub kind: NodeKind,
    /// Size of the data stream in bytes. `None` for folders and when the
    /// filesystem could not say.
    pub size: Option<u64>,
    /// Size of the resource fork in bytes. `None` for folders and where the
    /// format has no resource forks (see `VolumeInfo::resource_forks`).
    /// `Some(0)` for an empty fork, and for a transparently compressed file,
    /// whose resource fork holds the compressed content rather than a fork
    /// the user made.
    pub resource_size: Option<u64>,
    /// When the content was last modified, if known.
    pub modified: Option<Timestamp>,
    /// Cheap facts about the entry.
    pub flags: EntryFlags,
}

impl DirEntry<'_> {
    /// True when the entry can be opened as a folder: it is one, or it is a
    /// link to one.
    pub fn is_browsable(&self) -> bool {
        match self.kind {
            NodeKind::Directory => true,
            NodeKind::Symlink => self.flags.contains(EntryFlags::LINKS_TO_DIRECTORY),
            NodeKind::File | NodeKind::Other => false,
        }
    }
}

const HAS_SIZE: u8 = 1;
const HAS_MODIFIED: u8 = 1 << 1;
const HAS_RESOURCE_SIZE: u8 = 1 << 2;

/// The fixed-size part of an entry. The name is the slice of `DirBatch::names`
/// that ends at `name_end` and starts where the previous record's name ends.
#[derive(Debug, Clone, Copy)]
struct Record {
    node: NodeId,
    size: u64,
    resource_size: u64,
    modified: i64,
    name_end: usize,
    flags: EntryFlags,
    kind: NodeKind,
    present: u8,
}

/// A page of folder entries: every name in one buffer, plus one fixed-size
/// record per entry. Entries keep the order they were pushed in.
///
/// A batch is plain data. It is `Send + Sync`, cheap to move between
/// threads, and is shared behind an `Arc` once it is full.
#[derive(Debug, Clone, Default)]
pub struct DirBatch {
    names: String,
    records: Vec<Record>,
}

impl DirBatch {
    /// An empty batch.
    pub const fn new() -> Self {
        Self {
            names: String::new(),
            records: Vec::new(),
        }
    }

    /// An empty batch with room for `entries` entries and `name_bytes` bytes
    /// of names.
    pub fn with_capacity(entries: usize, name_bytes: usize) -> Self {
        Self {
            names: String::with_capacity(name_bytes),
            records: Vec::with_capacity(entries),
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when the batch holds no entries.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Appends an entry, copying its name into the batch.
    pub fn push(&mut self, entry: DirEntry<'_>) {
        self.names.push_str(entry.name);
        let mut present = 0;
        if entry.size.is_some() {
            present |= HAS_SIZE;
        }
        if entry.modified.is_some() {
            present |= HAS_MODIFIED;
        }
        if entry.resource_size.is_some() {
            present |= HAS_RESOURCE_SIZE;
        }
        self.records.push(Record {
            node: entry.node,
            size: entry.size.unwrap_or(0),
            resource_size: entry.resource_size.unwrap_or(0),
            modified: entry.modified.map_or(0, Timestamp::unix_nanos),
            name_end: self.names.len(),
            flags: entry.flags,
            kind: entry.kind,
            present,
        });
    }

    /// The entry at `index`, in push order.
    pub fn get(&self, index: usize) -> Option<DirEntry<'_>> {
        let record = self.records.get(index)?;
        let name_start = match index.checked_sub(1) {
            Some(previous) => self.records.get(previous)?.name_end,
            None => 0,
        };
        Some(DirEntry {
            node: record.node,
            name: self.names.get(name_start..record.name_end)?,
            kind: record.kind,
            size: (record.present & HAS_SIZE != 0).then_some(record.size),
            resource_size: (record.present & HAS_RESOURCE_SIZE != 0)
                .then_some(record.resource_size),
            modified: (record.present & HAS_MODIFIED != 0)
                .then_some(Timestamp::from_unix_nanos(record.modified)),
            flags: record.flags,
        })
    }

    /// The entries in push order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = DirEntry<'_>> + '_ {
        Iter {
            batch: self,
            next: 0,
        }
    }

    /// Removes every entry and keeps the allocations.
    pub fn clear(&mut self) {
        self.names.clear();
        self.records.clear();
    }
}

struct Iter<'a> {
    batch: &'a DirBatch,
    next: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = DirEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.batch.get(self.next)?;
        self.next = self.next.saturating_add(1);
        Some(entry)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.batch.len().saturating_sub(self.next);
        (left, Some(left))
    }
}

impl ExactSizeIterator for Iter<'_> {}

/// Whether a [`DirStream`] has more entries to give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirProgress {
    /// Call `next_batch` again.
    More,
    /// The folder has been listed to its end.
    Done,
}

/// An open folder listing. Dropping it abandons the listing.
///
/// A stream owns what it needs (it does not borrow the [`FileSystem`]), so
/// it can be moved to another thread between calls.
pub trait DirStream: Send {
    /// Appends up to `max` entries to `out`.
    ///
    /// Entries come in whatever order is cheapest for the format; the caller
    /// sorts. `.` and `..` are never reported. A call with `max == 0` appends
    /// nothing and returns `More`.
    ///
    /// On `Err`, the entries this call already appended to `out` are valid
    /// and stay there, so a damaged folder still shows what could be read.
    /// The stream is finished after an error: do not call it again.
    ///
    /// A problem with a single entry (its metadata cannot be read) is not an
    /// error: the entry is reported with `size` and `modified` unset.
    fn next_batch(&mut self, out: &mut DirBatch, max: usize) -> Result<DirProgress>;
}

/// Everything known about one node. Feeds the details pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The node described.
    pub node: NodeId,
    /// What the node is.
    pub kind: NodeKind,
    /// Size of the data stream in bytes. `None` for folders.
    pub size: Option<u64>,
    /// Size of the resource fork, as in [`DirEntry::resource_size`].
    pub resource_size: Option<u64>,
    /// When the node was created, if the filesystem records it.
    pub created: Option<Timestamp>,
    /// When the content was last modified.
    pub modified: Option<Timestamp>,
    /// When the content was last read.
    pub accessed: Option<Timestamp>,
    /// The same flags a listing reports for the node.
    pub flags: EntryFlags,
    /// For a symbolic link, the target as stored in the link.
    pub link_target: Option<String>,
    /// Classic Mac OS type and creator codes. `None` for folders, where the
    /// format has none, and when both codes are zero.
    pub finder_codes: Option<FinderCodes>,
    /// Owner and permissions as the volume records them. `None` where the
    /// format has none or did not record them (HFS+ written by Mac OS 9: mode 0).
    pub owner: Option<Owner>,
    /// Number of extended attributes, leaving out the ones the format keeps
    /// for itself (HFS+ `com.apple.decmpfs` on a compressed file, as macOS
    /// does). `None` where the format has none or they could not be read.
    pub attributes: Option<u32>,
    /// The 32 bytes of Finder information as stored: `FInfo` then `FXInfo`
    /// for a file, `DInfo` then `DXInfo` for a folder. `None` where the
    /// format has none and when all 32 bytes are zero. Copying out writes
    /// these into the AppleDouble sidecar unchanged.
    pub finder_info: Option<[u8; 32]>,
}

/// A file's classic Mac OS type and creator codes, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FinderCodes {
    /// The file type ("TEXT", "APPL").
    pub file_type: [u8; 4],
    /// The creating application ("ttxt").
    pub creator: [u8; 4],
}

/// Who owns a node and what they may do, as the volume records it. User
/// and group are numbers: names belong to the machine that wrote the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Owner {
    /// The owning user's ID.
    pub user: u32,
    /// The owning group's ID.
    pub group: u32,
    /// The BSD mode: file type bits and permission bits.
    pub mode: u16,
}

/// What mounting did with the volume's journal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum JournalState {
    /// No journal, an empty one, or one macOS would not replay (last
    /// mounted by a non-journaling implementation, or marked for
    /// initialization). The disk is current.
    Clean,
    /// `transactions` transactions were replayed in memory. `stopped_early`
    /// when a damaged block list ended the replay and only the block lists
    /// before the newest transaction start were applied, as xnu does (which
    /// can also leave out the complete transaction just before the damage).
    Replayed {
        /// Transactions applied.
        transactions: u32,
        /// A damaged transaction ended the replay.
        stopped_early: bool,
    },
    /// The journal has transactions that could not be replayed; the volume
    /// is read as it is on disk and may be inconsistent. `reason` is the
    /// error text (structure and location; byte offsets in the journal
    /// header and block lists count from the journal's start).
    NotReplayed {
        /// Why the journal was not replayed.
        reason: String,
    },
}

/// What the app shows about a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// The volume's name for display.
    pub name: String,
    /// The format's name as users know it ("HFS+", "APFS"). `None` when the
    /// format is the host's business, as it is for the host filesystem.
    pub format: Option<&'static str>,
    /// Size of the volume in bytes, if known.
    pub total_bytes: Option<u64>,
    /// Unused space in bytes, if known.
    pub free_bytes: Option<u64>,
    /// True when the format's files have resource forks (the HFS family).
    /// The file list shows a Resource fork column only then.
    pub resource_forks: bool,
    /// What mounting did with the volume's journal. `None` where the format
    /// has no journal or the reader does not handle one.
    pub journal: Option<JournalState>,
    /// True when the volume records dates as local time with no zone
    /// (classic HFS, FAT). Its [`Timestamp`]s then hold the local clock
    /// reading as if it were UTC: print their UTC fields as they are and do
    /// not convert them to the viewer's zone.
    pub local_times: bool,
}

impl VolumeInfo {
    /// A volume called `name` with nothing else known. Set the other fields
    /// afterwards.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            format: None,
            total_bytes: None,
            free_bytes: None,
            resource_forks: false,
            journal: None,
            local_times: false,
        }
    }
}

/// One stream of a node's content.
///
/// Extended attributes are not streams: they are small named values, read
/// whole through [`FileSystem::xattrs`] and [`FileSystem::read_xattr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    /// The data fork: what the file contains.
    Data,
    /// The resource fork (HFS family). Empty or absent elsewhere.
    Resource,
}

/// A run of bytes a stream occupies, in bytes of the source the filesystem
/// was mounted from (the partition, not a volume embedded in it). Add the
/// partition's start to place it on the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Extent {
    /// First byte of the run.
    pub offset: u64,
    /// Length of the run in bytes; never 0.
    pub len: u64,
}

/// One extended attribute of a node: its name and the size of its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XattrInfo {
    /// The attribute's name ("com.apple.FinderInfo").
    pub name: String,
    /// Size of the value in bytes.
    pub size: u64,
}

/// One snapshot of a volume: an earlier state that can be browsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// Names the snapshot to [`FileSystem::open_snapshot`]. Only meaningful
    /// to the filesystem that reported it.
    pub id: u64,
    /// The snapshot's name as stored.
    pub name: String,
    /// When the snapshot was taken, if recorded.
    pub created: Option<Timestamp>,
}

/// Most buckets [`FileSystem::allocation_map`] returns.
pub const MAX_ALLOCATION_BUCKETS: usize = 4096;

/// How full each slice of a volume is. Feeds the disk strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationMap {
    /// First byte of the mount source the buckets cover.
    pub start: u64,
    /// Bytes the buckets cover, from `start`: the volume's allocation space.
    pub len: u64,
    /// One value per equal slice of the covered bytes, in order: 0 when the
    /// slice is empty, 255 when it is full. A slice with any used space is
    /// at least 1, so a nearly empty region is still visible.
    pub density: Vec<u8>,
}

impl AllocationMap {
    /// A map of `density` over `len` bytes from `start`.
    pub fn new(start: u64, len: u64, density: Vec<u8>) -> Self {
        Self {
            start,
            len,
            density,
        }
    }
}

/// One entry reported by [`FileSystem::scan`].
#[derive(Debug, Clone, Copy)]
pub struct ScanEntry<'a> {
    /// The node to open or list. For a hard link, the target. A file system
    /// may instead report scan-local ids, valid only as `node` and `parent`
    /// links within this one scan (`NativeFs` does, from `FIRST_SCAN_ID`);
    /// then the entry is reached by walking its names with `lookup`.
    pub node: NodeId,
    /// The folder this entry is listed in.
    pub parent: NodeId,
    /// Display name, as a listing reports it.
    pub name: &'a str,
    /// What the entry is.
    pub kind: NodeKind,
    /// As in [`DirEntry::size`].
    pub size: Option<u64>,
    /// As in [`DirEntry::resource_size`].
    pub resource_size: Option<u64>,
    /// As in [`DirEntry::modified`].
    pub modified: Option<Timestamp>,
    /// As in [`DirEntry::flags`].
    pub flags: EntryFlags,
    /// The classic Mac file type (`FInfo.fdType`), when the format keeps one
    /// and it is not four zero bytes. Feeds the file-kind classifier for names without
    /// an extension. `None` for folders and on formats without type codes.
    pub finder_type: Option<[u8; 4]>,
}

/// What a scan covered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanSummary {
    /// Entries passed to the visitor.
    pub entries: u64,
    /// Structures (B-tree nodes, folders) that could not be read and were skipped.
    pub skipped: u64,
}

/// Names a volume and the state it is in, for the saved search index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VolumeIdentity {
    /// Stays the same for the life of the volume.
    pub id: [u8; 16],
    /// Changes whenever the volume's content may have changed.
    pub change: [u8; 16],
}

/// Most folders [`default_scan`] queues before it gives up.
pub const MAX_SCAN_FOLDERS: u64 = 50_000_000;

/// Calls in a row that [`default_scan`] lets a folder's stream answer
/// `More` without adding an entry before it skips the folder. Matches the
/// app loader's bound.
pub const MAX_EMPTY_BATCHES: u32 = 64;

/// Entries [`default_scan`] asks a stream for per call.
const SCAN_BATCH: usize = 4096;

/// [`FileSystem::scan`] through `read_dir`: breadth-first from the root.
///
/// Every folder is listed once (a set of queued folders guards against
/// cycles). Symbolic links are reported and never followed. A folder whose
/// `read_dir` or `next_batch` fails counts as one skipped structure; the
/// entries the failing call already appended are still visited. Fails with
/// `Corrupt(LimitExceeded)` past [`MAX_SCAN_FOLDERS`] queued folders.
pub fn default_scan<F: FileSystem + ?Sized>(
    fs: &F,
    visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>,
) -> Result<ScanSummary> {
    let mut summary = ScanSummary::default();
    let root = fs.root();
    let mut queued: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<NodeId> = VecDeque::new();
    queued.insert(root);
    queue.push_back(root);
    let mut folders: u64 = 1;
    let mut batch = DirBatch::with_capacity(SCAN_BATCH, SCAN_BATCH.saturating_mul(24));

    while let Some(dir) = queue.pop_front() {
        let mut stream = match fs.read_dir(dir) {
            Ok(stream) => stream,
            Err(_) => {
                summary.skipped = summary.skipped.saturating_add(1);
                continue;
            }
        };
        let mut empty_in_a_row: u32 = 0;
        loop {
            batch.clear();
            let progress = stream.next_batch(&mut batch, SCAN_BATCH);
            for entry in batch.iter() {
                summary.entries = summary.entries.saturating_add(1);
                let flow = visit(ScanEntry {
                    node: entry.node,
                    parent: dir,
                    name: entry.name,
                    kind: entry.kind,
                    size: entry.size,
                    resource_size: entry.resource_size,
                    modified: entry.modified,
                    flags: entry.flags,
                    finder_type: None,
                });
                if flow.is_break() {
                    return Ok(summary);
                }
                if entry.kind == NodeKind::Directory && queued.insert(entry.node) {
                    if folders >= MAX_SCAN_FOLDERS {
                        return Err(Corruption::new(
                            "folder tree",
                            Location::Unspecified,
                            CorruptKind::LimitExceeded {
                                what: "folders queued by the scan",
                                limit: MAX_SCAN_FOLDERS,
                            },
                        )
                        .into());
                    }
                    folders = folders.saturating_add(1);
                    queue.push_back(entry.node);
                }
            }
            match progress {
                Ok(DirProgress::Done) => break,
                Ok(DirProgress::More) if batch.is_empty() => {
                    empty_in_a_row = empty_in_a_row.saturating_add(1);
                    if empty_in_a_row >= MAX_EMPTY_BATCHES {
                        summary.skipped = summary.skipped.saturating_add(1);
                        break;
                    }
                }
                Ok(DirProgress::More) => empty_in_a_row = 0,
                Err(_) => {
                    summary.skipped = summary.skipped.saturating_add(1);
                    break;
                }
            }
        }
    }
    Ok(summary)
}

/// A read-only filesystem.
///
/// Implementations are blocking and re-entrant: any number of threads may
/// call any method at once. None of these methods may be called on the UI
/// thread, except [`Self::info`].
pub trait FileSystem: Send + Sync + 'static {
    /// Describes the volume. Answers from memory, with no I/O and no lock
    /// held across I/O, so it is the one method the UI thread may call.
    fn info(&self) -> VolumeInfo;

    /// The top folder.
    fn root(&self) -> NodeId;

    /// Everything known about `node`.
    fn metadata(&self, node: NodeId) -> Result<Metadata>;

    /// The entry called `name` in the folder `dir`, compared the way the
    /// format compares names. `Ok(None)` when there is no such entry.
    fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>>;

    /// Opens the folder `dir` for listing. A symbolic link to a folder may be
    /// passed where the format can follow it.
    ///
    /// Fails with `Error::NotADirectory` when `dir` is not a folder and
    /// `Error::NotFound` when it does not exist.
    fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>>;

    /// Where `stream` of `node` lives on disk, in file order, with runs that
    /// touch on disk merged. Feeds the disk strip's selection marks.
    ///
    /// Hard links report their target. Empty for an empty stream, and for
    /// content not stored in blocks of its own (data kept inside a metadata
    /// structure). For [`Stream::Data`] of a transparently compressed file,
    /// the runs that hold its compressed content.
    ///
    /// Fails with `Error::IsADirectory` for a folder, `Error::NotFound` for a
    /// missing node, and `Error::Unsupported` where the format cannot say
    /// (the default).
    fn extents(&self, node: NodeId, stream: Stream) -> Result<Vec<Extent>> {
        let _ = (node, stream);
        Err(crate::error::Error::Unsupported("extents".into()))
    }

    /// How full the volume is in up to `buckets` equal slices, clamped to
    /// 1 ..= [`MAX_ALLOCATION_BUCKETS`] and to the volume's allocation units.
    /// Feeds the disk strip's shading.
    ///
    /// Reads the volume's whole allocation record (64 MiB of bitmap for a
    /// 2 TB HFS+ volume of 4 KiB blocks), so call it once per volume and keep
    /// the result. Fails with `Error::Unsupported` where the format cannot
    /// say (the default).
    fn allocation_map(&self, buckets: usize) -> Result<AllocationMap> {
        let _ = buckets;
        Err(crate::error::Error::Unsupported("allocation map".into()))
    }

    /// The content of `stream` of `node` as a readable source. Its
    /// `sector_size` is 512 and means nothing.
    ///
    /// [`Stream::Data`] of a transparently compressed file is the
    /// decompressed content. Hard links give their target. A symbolic link
    /// gives the stored target path.
    ///
    /// Fails with `Error::IsADirectory` for a folder, `Error::NotFound` for
    /// a missing node, and `Error::NoSuchStream` for [`Stream::Resource`]
    /// where the format has no forks, where the fork is empty, and for a
    /// compressed file (its fork is how the content is stored, not a fork
    /// the user made). `Error::Unsupported` where the format's reader cannot
    /// open content (the default).
    ///
    /// Opening may read metadata (extent records, a compression header) but
    /// never the content itself: a damaged file fails in `read_at`, at the
    /// damage, so its readable part stays readable.
    fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>> {
        let _ = (node, stream);
        Err(Error::Unsupported("opening file content".into()))
    }

    /// Names and sizes of the node's extended attributes (of the target,
    /// for a hard link), leaving out the ones the format keeps for itself.
    /// Empty where the format has none (the default).
    fn xattrs(&self, node: NodeId) -> Result<Vec<XattrInfo>> {
        let _ = node;
        Ok(Vec::new())
    }

    /// The whole value of the attribute `name` of `node`. `Ok(None)` when
    /// the node has no such attribute, including the ones [`Self::xattrs`]
    /// leaves out. Fails with `Error::Unsupported` when the value is longer
    /// than `max` bytes; nothing that large is read or allocated.
    fn read_xattr(&self, node: NodeId, name: &str, max: usize) -> Result<Option<Vec<u8>>> {
        let _ = (node, name, max);
        Ok(None)
    }

    /// The volume's snapshots, in no particular order. Empty where the
    /// format has none (the default). May read from disk.
    fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        Ok(Vec::new())
    }

    /// The volume as it was at the snapshot `id` (from [`Self::snapshots`]),
    /// as a filesystem of its own. Its node ids are not interchangeable with
    /// this one's. Fails with `Error::NotFound` for an id that names no
    /// snapshot and `Error::Unsupported` where the format has none (the
    /// default) or the snapshot holds no data.
    fn open_snapshot(&self, id: u64) -> Result<Arc<dyn FileSystem>> {
        let _ = id;
        Err(Error::Unsupported("snapshots".into()))
    }

    /// Every entry on the volume except the root, in whatever order is fastest.
    ///
    /// Contract: children may arrive before their parents. A node may be
    /// reported more than once under different parents (hard links); for a
    /// folder, its children hang from its first report. The root is never
    /// reported; its children have `parent == self.root()`. Entries a listing
    /// hides (format-private folders) are not reported. The visitor returns
    /// `Break` to stop; the scan then returns `Ok` with what it had counted.
    /// A damaged structure is skipped and counted, not an error; `Err` is for
    /// I/O failure and for a volume that cannot be scanned at all.
    ///
    /// The default walks `read_dir` breadth-first from the root
    /// ([`default_scan`]).
    fn scan(&self, visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>) -> Result<ScanSummary> {
        default_scan(self, visit)
    }

    /// The volume's identity and change marker. Answers from memory.
    /// `None` when the volume has none or may change while open (the host).
    fn identity(&self) -> Option<VolumeIdentity> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, size: Option<u64>, modified: Option<Timestamp>) -> DirEntry<'_> {
        DirEntry {
            node: NodeId(7),
            name,
            kind: NodeKind::File,
            size,
            resource_size: None,
            modified,
            flags: EntryFlags::EMPTY,
        }
    }

    #[test]
    fn batch_round_trip() {
        let epoch = Timestamp::from_unix_nanos(0);
        let entries = [
            entry("first", Some(10), Some(epoch)),
            entry("", None, None),
            entry("h\u{e9}llo \u{4e16}\u{754c} \u{1f600}", Some(0), None),
            DirEntry {
                node: NodeId(9),
                name: "dir",
                kind: NodeKind::Directory,
                size: None,
                resource_size: None,
                modified: Some(Timestamp::from_unix_nanos(-5)),
                flags: EntryFlags::HIDDEN.with(EntryFlags::LINKS_TO_DIRECTORY),
            },
            DirEntry {
                resource_size: Some(0),
                ..entry("empty fork", Some(3), None)
            },
            DirEntry {
                resource_size: Some(4096),
                flags: EntryFlags::COMPRESSED,
                ..entry("compressed", Some(9_000), None)
            },
        ];
        let mut batch = DirBatch::with_capacity(2, 8);
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
        for e in entries {
            batch.push(e);
        }
        assert!(!batch.is_empty());
        assert_eq!(batch.len(), entries.len());
        for (index, expected) in entries.iter().enumerate() {
            assert_eq!(batch.get(index), Some(*expected));
        }
        assert_eq!(batch.get(entries.len()), None);
        assert_eq!(batch.get(usize::MAX), None);
        assert!(batch.iter().eq(entries.iter().copied()));
        // Some(0) and Some(epoch) are distinct from None.
        assert_eq!(batch.get(2).unwrap().size, Some(0));
        assert_eq!(batch.get(1).unwrap().size, None);
        assert_eq!(batch.get(0).unwrap().modified, Some(epoch));
        assert_eq!(batch.get(2).unwrap().modified, None);
        // The resource size has its own presence bit, apart from `size`.
        assert_eq!(batch.get(0).unwrap().resource_size, None);
        assert_eq!(batch.get(3).unwrap().resource_size, None);
        assert_eq!(batch.get(4).unwrap().resource_size, Some(0));
        assert_eq!(batch.get(5).unwrap().resource_size, Some(4096));
        assert!(batch.get(5).unwrap().flags.contains(EntryFlags::COMPRESSED));
        assert!(!batch.get(4).unwrap().flags.contains(EntryFlags::COMPRESSED));

        {
            let mut iter = batch.iter();
            assert_eq!(iter.len(), 6);
            iter.next();
            assert_eq!(iter.len(), 5);
            assert_eq!(iter.size_hint(), (5, Some(5)));
            iter.by_ref().for_each(drop);
            assert_eq!(iter.len(), 0);
        }

        batch.clear();
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
        assert_eq!(batch.get(0), None);
        assert_eq!(batch.iter().len(), 0);
        // Reusable after clear.
        batch.push(entry("again", None, None));
        assert_eq!(batch.get(0).unwrap().name, "again");
    }

    #[test]
    fn entry_flags() {
        let none = EntryFlags::EMPTY;
        assert!(none.is_empty());
        assert!(EntryFlags::default().is_empty());
        assert!(none.contains(none));
        assert!(!none.contains(EntryFlags::HIDDEN));

        let hidden = none.with(EntryFlags::HIDDEN);
        assert!(!hidden.is_empty());
        assert!(hidden.contains(EntryFlags::HIDDEN));
        assert!(!hidden.contains(EntryFlags::LINKS_TO_DIRECTORY));

        let both = hidden.with(EntryFlags::LINKS_TO_DIRECTORY);
        assert!(both.contains(EntryFlags::HIDDEN));
        assert!(both.contains(EntryFlags::LINKS_TO_DIRECTORY));
        assert!(both.contains(hidden));
        assert!(!hidden.contains(both));
        assert_eq!(both.with(EntryFlags::HIDDEN), both);
    }

    #[test]
    fn package_alias_and_label() {
        let all = [
            EntryFlags::HIDDEN,
            EntryFlags::LINKS_TO_DIRECTORY,
            EntryFlags::COMPRESSED,
            EntryFlags::PACKAGE,
            EntryFlags::ALIAS,
        ];
        // Distinct single bits.
        for (i, a) in all.iter().enumerate() {
            assert_eq!(a.0.count_ones(), 1);
            for b in &all[i + 1..] {
                assert!(!a.contains(*b));
            }
        }
        assert_eq!(EntryFlags::PACKAGE.0, 1 << 3);
        assert_eq!(EntryFlags::ALIAS.0, 1 << 4);

        assert_eq!(EntryFlags::EMPTY.label(), 0);
        let flags = EntryFlags::PACKAGE.with(EntryFlags::ALIAS);
        for label in 0..=7u8 {
            let f = flags.with_label(label);
            assert_eq!(f.label(), label);
            assert!(f.contains(EntryFlags::PACKAGE) && f.contains(EntryFlags::ALIAS));
            assert_eq!(f.with_label(0), flags);
            assert!(!f.is_empty());
            // Bits 8 to 10 only.
            assert_eq!(f.0 & !0b0111_0001_1111, 0);
        }
        // Only the low three bits are kept; the rest of the word is untouched.
        assert_eq!(EntryFlags::EMPTY.with_label(0xFF).label(), 7);
        assert_eq!(EntryFlags::EMPTY.with_label(8).label(), 0);
        assert_eq!(
            EntryFlags::HIDDEN.with_label(0xF9),
            EntryFlags::HIDDEN.with_label(1)
        );
        assert!(!EntryFlags::EMPTY.with_label(3).is_empty());
    }

    #[test]
    fn timestamp_saturates() {
        assert_eq!(
            Timestamp::from_unix(i64::MAX, 999_999_999).unix_nanos(),
            i64::MAX
        );
        assert_eq!(Timestamp::from_unix(i64::MIN, 0).unix_nanos(), i64::MIN);
        assert_eq!(Timestamp::from_unix(1, 5).unix_nanos(), 1_000_000_005);
        assert_eq!(Timestamp::from_unix(-1, 0).unix_nanos(), -1_000_000_000);
        // Accessors on the extremes do not panic.
        let _ = Timestamp::from_unix_nanos(i64::MIN).unix_seconds();
        let _ = Timestamp::from_unix_nanos(i64::MIN).subsec_nanos();
        let _ = Timestamp::from_unix_nanos(i64::MAX).unix_seconds();
    }

    #[test]
    fn timestamp_parts_round_toward_the_past() {
        let t = Timestamp::from_unix_nanos(-1);
        assert_eq!(t.unix_seconds(), -1);
        assert_eq!(t.subsec_nanos(), 999_999_999);
        let t = Timestamp::from_unix_nanos(-1_000_000_000);
        assert_eq!(t.unix_seconds(), -1);
        assert_eq!(t.subsec_nanos(), 0);
        let t = Timestamp::from_unix_nanos(1_500_000_000);
        assert_eq!(t.unix_seconds(), 1);
        assert_eq!(t.subsec_nanos(), 500_000_000);
    }

    #[test]
    fn timestamp_from_system_time() {
        let after = Timestamp::from_system_time(UNIX_EPOCH + Duration::new(2, 100));
        assert_eq!(after.unix_nanos(), 2_000_000_100); // 100 ns: the Windows clock resolution
        let before = Timestamp::from_system_time(UNIX_EPOCH - Duration::new(1, 500_000_000));
        assert_eq!(before.unix_nanos(), -1_500_000_000);
        assert_eq!(before.unix_seconds(), -2);
        assert_eq!(before.subsec_nanos(), 500_000_000);
        assert_eq!(Timestamp::from_system_time(UNIX_EPOCH).unix_nanos(), 0);
    }

    /// A tree in memory for `default_scan`: folder id → entries. Folder 1 is
    /// the root. A folder listed in `broken` fails `read_dir`; one in
    /// `fails_midway` returns its first entry, then an error.
    struct TreeFs {
        folders: std::collections::HashMap<u64, Vec<(u64, &'static str, NodeKind)>>,
        broken: Vec<u64>,
        fails_midway: Vec<u64>,
    }

    struct TreeStream {
        entries: Vec<(u64, &'static str, NodeKind)>,
        fail_after_first: bool,
        done: bool,
    }

    impl DirStream for TreeStream {
        fn next_batch(&mut self, out: &mut DirBatch, _max: usize) -> Result<DirProgress> {
            if self.done {
                return Ok(DirProgress::Done);
            }
            let take = if self.fail_after_first {
                1
            } else {
                self.entries.len()
            };
            for &(node, name, kind) in self.entries.iter().take(take) {
                out.push(DirEntry {
                    node: NodeId(node),
                    name,
                    kind,
                    size: None,
                    resource_size: None,
                    modified: None,
                    flags: EntryFlags::EMPTY,
                });
            }
            self.done = true;
            if self.fail_after_first {
                Err(crate::error::Error::unsupported("damaged"))
            } else {
                Ok(DirProgress::More)
            }
        }
    }

    impl FileSystem for TreeFs {
        fn info(&self) -> VolumeInfo {
            VolumeInfo::new("tree")
        }
        fn root(&self) -> NodeId {
            NodeId(1)
        }
        fn metadata(&self, node: NodeId) -> Result<Metadata> {
            Err(crate::error::Error::NotFound { node: node.0 })
        }
        fn lookup(&self, _dir: NodeId, _name: &str) -> Result<Option<NodeId>> {
            Ok(None)
        }
        fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
            if self.broken.contains(&dir.0) {
                return Err(crate::error::Error::unsupported("broken"));
            }
            let entries = self
                .folders
                .get(&dir.0)
                .cloned()
                .ok_or(crate::error::Error::NotADirectory { node: dir.0 })?;
            Ok(Box::new(TreeStream {
                entries,
                fail_after_first: self.fails_midway.contains(&dir.0),
                done: false,
            }))
        }
    }

    fn tree() -> TreeFs {
        use NodeKind::{Directory as D, File as F, Symlink as L};
        let mut folders = std::collections::HashMap::new();
        folders.insert(
            1,
            vec![(2, "a", D), (3, "f", F), (4, "link", L), (6, "bad", D)],
        );
        // 5 lists the root and its own parent again: a cycle.
        folders.insert(2, vec![(5, "b", D), (7, "g", F)]);
        folders.insert(5, vec![(1, "up", D), (2, "again", D), (8, "half", D)]);
        folders.insert(8, vec![(9, "first", F), (10, "lost", F)]);
        TreeFs {
            folders,
            broken: vec![6],
            fails_midway: vec![8],
        }
    }

    #[test]
    fn default_scan_walks_once_and_counts_damage() {
        let fs = tree();
        let mut seen = Vec::new();
        let summary = fs
            .scan(&mut |e| {
                seen.push((e.parent.0, e.node.0, e.name.to_owned()));
                ControlFlow::Continue(())
            })
            .unwrap();
        // Breadth-first; the cycle back to 1 and 2 is reported but not walked again.
        let expected: Vec<(u64, u64, String)> = [
            (1, 2, "a"),
            (1, 3, "f"),
            (1, 4, "link"),
            (1, 6, "bad"),
            (2, 5, "b"),
            (2, 7, "g"),
            (5, 1, "up"),
            (5, 2, "again"),
            (5, 8, "half"),
            (8, 9, "first"),
        ]
        .into_iter()
        .map(|(p, n, s)| (p, n, s.to_owned()))
        .collect();
        assert_eq!(seen, expected);
        assert_eq!(summary.entries, 10);
        // Folder 6 fails to open; folder 8 fails after its first entry.
        assert_eq!(summary.skipped, 2);
    }

    #[test]
    fn default_scan_stops_on_break() {
        let fs = tree();
        let mut count = 0;
        let summary = fs
            .scan(&mut |_| {
                count += 1;
                if count == 3 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(summary.entries, 3);
        assert_eq!(fs.identity(), None);
    }

    #[test]
    fn default_scan_gives_up_on_a_stream_that_never_ends() {
        struct Endless;
        impl DirStream for Endless {
            fn next_batch(&mut self, _out: &mut DirBatch, _max: usize) -> Result<DirProgress> {
                Ok(DirProgress::More)
            }
        }
        struct EndlessFs;
        impl FileSystem for EndlessFs {
            fn info(&self) -> VolumeInfo {
                VolumeInfo::new("endless")
            }
            fn root(&self) -> NodeId {
                NodeId(1)
            }
            fn metadata(&self, node: NodeId) -> Result<Metadata> {
                Err(crate::error::Error::NotFound { node: node.0 })
            }
            fn lookup(&self, _dir: NodeId, _name: &str) -> Result<Option<NodeId>> {
                Ok(None)
            }
            fn read_dir(&self, _dir: NodeId) -> Result<Box<dyn DirStream>> {
                Ok(Box::new(Endless))
            }
        }
        let summary = EndlessFs.scan(&mut |_| ControlFlow::Continue(())).unwrap();
        assert_eq!(
            summary,
            ScanSummary {
                entries: 0,
                skipped: 1
            }
        );
    }

    #[test]
    fn is_browsable_by_kind() {
        let of = |kind, flags| DirEntry {
            node: NodeId(1),
            name: "x",
            kind,
            size: None,
            resource_size: None,
            modified: None,
            flags,
        };
        let none = EntryFlags::EMPTY;
        let link = EntryFlags::LINKS_TO_DIRECTORY;
        assert!(of(NodeKind::Directory, none).is_browsable());
        assert!(!of(NodeKind::File, none).is_browsable());
        assert!(!of(NodeKind::Other, none).is_browsable());
        assert!(!of(NodeKind::Symlink, none).is_browsable());
        assert!(!of(NodeKind::Symlink, EntryFlags::HIDDEN).is_browsable());
        assert!(of(NodeKind::Symlink, link).is_browsable());
        // The flag means nothing on a file.
        assert!(!of(NodeKind::File, link).is_browsable());
    }

    /// A format that implements only the required methods answers the rest
    /// with "cannot say" or "has none", never with a panic.
    #[test]
    fn optional_methods_have_defaults() {
        use crate::error::Error;
        let fs = tree();
        let node = NodeId(1);
        assert!(matches!(
            fs.open(node, Stream::Data),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            fs.open(node, Stream::Resource),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(fs.xattrs(node).unwrap(), Vec::new());
        assert_eq!(
            fs.read_xattr(node, "com.apple.FinderInfo", 64).unwrap(),
            None
        );
        assert_eq!(fs.snapshots().unwrap(), Vec::new());
        assert!(matches!(fs.open_snapshot(7), Err(Error::Unsupported(_))));
        assert!(matches!(
            fs.extents(node, Stream::Data),
            Err(Error::Unsupported(_))
        ));
        assert!(fs.identity().is_none());
        assert!(!fs.info().local_times);
    }
}
