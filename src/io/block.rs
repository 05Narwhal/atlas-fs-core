// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! `BlockSource`, sub-range slices and an in-memory source.
//!
//! A source stack is built bottom up: a base source (a host file, a device,
//! a decoded disk image), then one `CachedSource`
//! over it, then a [`Slice`] per partition. Every partition of a disk then
//! shares one cache keyed by the base source's chunks.

use std::sync::Arc;

use crate::error::{Error, Result};

/// Positional, thread-safe reads over a fixed-size run of bytes.
///
/// There is no shared cursor: any number of threads may call `read_at` at
/// once. The size does not change while the source is open.
pub trait BlockSource: Send + Sync {
    /// Size in bytes.
    fn len(&self) -> u64;

    /// True when the source holds no bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The device's logical sector size in bytes: a power of two from 512
    /// to 65536. Partition maps count in it. Sources that are not devices
    /// report 512.
    fn sector_size(&self) -> u32;

    /// Fills `buf` completely with the bytes at `offset`, or fails.
    ///
    /// Any byte range is allowed; a source that needs aligned access (a raw
    /// device) aligns internally. A range that reaches past `len()`, or whose
    /// end overflows `u64`, fails with [`Error::OutOfBounds`] and reads
    /// nothing. An empty `buf` at any `offset <= len()` succeeds.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Same contract as `read_at`, for large sequential reads (index builds,
    /// copying out). A cache passes these straight through so a scan does
    /// not evict the blocks the interface is using.
    fn read_bulk(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.read_at(offset, buf)
    }
}

/// Checks that `len` bytes at `offset` lie inside a source of `size` bytes.
///
/// Every `BlockSource` implementation calls this first.
pub fn check_range(offset: u64, len: usize, size: u64) -> Result<()> {
    let len = u64::try_from(len).unwrap_or(u64::MAX);
    match offset.checked_add(len) {
        Some(end) if end <= size => Ok(()),
        _ => Err(Error::OutOfBounds { offset, len, size }),
    }
}

/// A sub-range of another source, such as one partition of a disk.
///
/// Offsets passed to a slice are relative to its start. A slice never reads
/// outside its range.
#[derive(Clone)]
pub struct Slice {
    inner: Arc<dyn BlockSource>,
    start: u64,
    len: u64,
}

impl Slice {
    /// The `len` bytes of `inner` that begin at `start`.
    ///
    /// Fails with [`Error::OutOfBounds`] when the range does not lie inside
    /// `inner`. A caller that wants to keep a partition that runs past the
    /// end of a truncated image clamps `len` first.
    pub fn new(inner: Arc<dyn BlockSource>, start: u64, len: u64) -> Result<Self> {
        let size = inner.len();
        match start.checked_add(len) {
            Some(end) if end <= size => Ok(Self { inner, start, len }),
            _ => Err(Error::OutOfBounds {
                offset: start,
                len,
                size,
            }),
        }
    }

    /// Byte offset of the slice in the source it was cut from.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Maps a slice offset to an inner offset after checking the range.
    fn map(&self, offset: u64, len: usize) -> Result<u64> {
        check_range(offset, len, self.len)?;
        // Cannot overflow: offset <= self.len and start + self.len <= inner.len().
        self.start.checked_add(offset).ok_or(Error::OutOfBounds {
            offset,
            len: u64::try_from(len).unwrap_or(u64::MAX),
            size: self.len,
        })
    }
}

impl BlockSource for Slice {
    fn len(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let at = self.map(offset, buf.len())?;
        self.inner.read_at(at, buf)
    }

    fn read_bulk(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let at = self.map(offset, buf.len())?;
        self.inner.read_bulk(at, buf)
    }
}

/// A source held in memory: test fixtures and small decoded images.
pub struct MemSource {
    bytes: Arc<[u8]>,
    sector_size: u32,
}

impl MemSource {
    /// `bytes` as a source with 512-byte sectors.
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self::with_sector_size(bytes, 512)
    }

    /// `bytes` as a source that reports `sector_size`, for modelling disks
    /// with 4096-byte sectors. The caller keeps it a power of two from 512
    /// to 65536, as the trait requires.
    pub fn with_sector_size(bytes: impl Into<Arc<[u8]>>, sector_size: u32) -> Self {
        Self {
            bytes: bytes.into(),
            sector_size,
        }
    }
}

impl BlockSource for MemSource {
    fn len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len())?;
        let start = usize::try_from(offset).map_err(|_| Error::OutOfBounds {
            offset,
            len: u64::try_from(buf.len()).unwrap_or(u64::MAX),
            size: self.len(),
        })?;
        let src = start
            .checked_add(buf.len())
            .and_then(|end| self.bytes.get(start..end))
            .ok_or(Error::OutOfBounds {
                offset,
                len: u64::try_from(buf.len()).unwrap_or(u64::MAX),
                size: self.len(),
            })?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(len: usize) -> Arc<MemSource> {
        Arc::new(MemSource::new(
            (0..len).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
        ))
    }

    fn is_oob(r: Result<()>) -> bool {
        matches!(r, Err(Error::OutOfBounds { .. }))
    }

    #[test]
    fn check_range_edges() {
        assert!(check_range(0, 100, 100).is_ok());
        assert!(check_range(90, 10, 100).is_ok());
        assert!(is_oob(check_range(90, 11, 100)));
        assert!(is_oob(check_range(0, 101, 100)));
        assert!(check_range(100, 0, 100).is_ok());
        assert!(is_oob(check_range(101, 0, 100)));
        assert!(is_oob(check_range(u64::MAX, 1, u64::MAX)));
        assert!(check_range(u64::MAX, 0, u64::MAX).is_ok());
        assert!(is_oob(check_range(u64::MAX - 1, 2, u64::MAX)));
        assert!(check_range(0, 0, 0).is_ok());
        match check_range(90, 11, 100) {
            Err(Error::OutOfBounds { offset, len, size }) => {
                assert_eq!((offset, len, size), (90, 11, 100));
            }
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
    }

    #[test]
    fn slice_new_rejects_bad_ranges() {
        let src = mem(1000);
        assert!(Slice::new(src.clone(), 0, 1000).is_ok());
        assert!(Slice::new(src.clone(), 1000, 0).is_ok());
        assert!(Slice::new(src.clone(), 500, 501).is_err());
        assert!(Slice::new(src.clone(), 1001, 0).is_err());
        assert!(Slice::new(src.clone(), u64::MAX, 2).is_err());
        assert!(Slice::new(src, 1, u64::MAX).is_err());
    }

    #[test]
    fn slice_maps_offsets_and_stays_inside() {
        let src = mem(1000);
        let slice = Slice::new(src.clone(), 300, 200).unwrap();
        assert_eq!(slice.len(), 200);
        assert_eq!(slice.start(), 300);
        assert_eq!(slice.sector_size(), 512);

        let mut got = [0u8; 50];
        let mut want = [0u8; 50];
        slice.read_at(10, &mut got).unwrap();
        src.read_at(310, &mut want).unwrap();
        assert_eq!(got, want);
        slice.read_bulk(150, &mut got).unwrap();
        src.read_at(450, &mut want).unwrap();
        assert_eq!(got, want);

        // The inner source has bytes past the slice; the slice refuses them.
        assert!(is_oob(slice.read_at(151, &mut got)));
        assert!(is_oob(slice.read_bulk(151, &mut got)));
        assert!(is_oob(slice.read_at(200, &mut [0u8; 1])));
        assert!(slice.read_at(200, &mut []).is_ok());
        assert!(is_oob(slice.read_at(u64::MAX, &mut [0u8; 1])));

        // A slice of a slice composes offsets.
        let inner: Arc<dyn BlockSource> = Arc::new(slice);
        let nested = Slice::new(inner, 20, 30).unwrap();
        let mut got = [0u8; 30];
        let mut want = [0u8; 30];
        nested.read_at(0, &mut got).unwrap();
        src.read_at(320, &mut want).unwrap();
        assert_eq!(got, want);
        assert!(is_oob(nested.read_at(1, &mut got)));
    }

    #[test]
    fn mem_source_reads() {
        let src = MemSource::with_sector_size(vec![1u8, 2, 3, 4, 5], 4096);
        assert_eq!(src.len(), 5);
        assert!(!src.is_empty());
        assert_eq!(src.sector_size(), 4096);
        let mut buf = [0u8; 3];
        src.read_at(1, &mut buf).unwrap();
        assert_eq!(buf, [2, 3, 4]);
        src.read_bulk(2, &mut buf).unwrap();
        assert_eq!(buf, [3, 4, 5]);
        assert!(src.read_at(5, &mut []).is_ok());
        assert!(is_oob(src.read_at(6, &mut [])));
        let mut buf = [9u8; 3];
        assert!(is_oob(src.read_at(3, &mut buf)));
        assert_eq!(buf, [9, 9, 9]);
        assert!(MemSource::new(Vec::new()).is_empty());
    }
}
