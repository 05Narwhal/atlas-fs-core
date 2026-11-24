// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! Image files read through positional reads.
//!
//! [`FileSource`] opens a host file read-only and reads it with `pread` on
//! Unix and `ReadFile` with an offset on Windows. There is no shared cursor,
//! so any number of threads may read at once. The file is never memory
//! mapped: a truncated or removed file must turn into an error, not a crash.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, IoContext, Result};
use crate::io::block::{BlockSource, check_range};

/// A host file, such as a raw disk image, as a [`BlockSource`].
///
/// The size is taken once at open, as the trait requires. If the file
/// shrinks afterwards, reads past its new end fail with an I/O error.
pub struct FileSource {
    file: File,
    path: PathBuf,
    len: u64,
}

impl FileSource {
    /// Opens `path` read-only.
    ///
    /// Fails with an I/O error in the "opening" context when the path does
    /// not exist, cannot be read, or is a directory.
    pub fn open(path: &Path) -> Result<Self> {
        let context = || IoContext::Path {
            op: "opening",
            path: path.to_path_buf(),
        };
        let is_dir_error = || io::Error::from(io::ErrorKind::IsADirectory);
        let file = File::open(path).map_err(|e| {
            // Windows refuses to open a directory as a file with "access
            // denied"; say what is actually wrong.
            let e = if path.is_dir() { is_dir_error() } else { e };
            Error::io(context(), e)
        })?;
        let meta = file.metadata().map_err(|e| Error::io(context(), e))?;
        if meta.is_dir() {
            return Err(Error::io(context(), is_dir_error()));
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            len: meta.len(),
        })
    }

    /// The path the file was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl BlockSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        if buf.is_empty() {
            return Ok(());
        }
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        read_exact_at(&self.file, buf, offset)
            .map_err(|e| Error::io(IoContext::Read { offset, len }, e))
    }
}

/// Fills `buf` from `offset`, or fails. A file that ends early fails with
/// `UnexpectedEof`.
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

/// Fills `buf` from `offset`, or fails. A file that ends early fails with
/// `UnexpectedEof`.
///
/// `seek_read` also moves the handle's cursor; nothing here uses the cursor.
#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    // Every successful pass shortens `buf` by at least one byte; a 0-byte
    // read ends the loop with an error.
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file ended before the read was filled",
                ));
            }
            Ok(n) => {
                let advanced = u64::try_from(n).ok();
                let rest = std::mem::take(&mut buf).get_mut(n..);
                match (rest, advanced.and_then(|a| offset.checked_add(a))) {
                    (Some(rest), Some(next)) => {
                        buf = rest;
                        offset = next;
                    }
                    _ => return Err(io::Error::other("read returned more bytes than asked")),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Positional reads are only implemented for Unix and Windows hosts.
#[cfg(not(any(unix, windows)))]
fn read_exact_at(_file: &File, _buf: &mut [u8], _offset: u64) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique path under the system temp directory.
    fn temp_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "disk-atlas-file-test-{}-{n}.bin",
            std::process::id()
        ))
    }

    /// Removes the file when dropped, so a failing assertion still cleans up.
    struct TempFile(PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn temp_file(bytes: &[u8]) -> TempFile {
        let path = temp_path();
        std::fs::write(&path, bytes).unwrap();
        TempFile(path)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn reads_ranges_back() {
        let data = pattern(200_000);
        let tmp = temp_file(&data);
        let src = FileSource::open(&tmp.0).unwrap();
        assert_eq!(src.len(), data.len() as u64);
        assert_eq!(src.sector_size(), 512);
        assert_eq!(src.path(), tmp.0.as_path());
        for (offset, len) in [
            (0usize, 0usize),
            (0, 1),
            (0, 512),
            (511, 2),
            (65_535, 70_000),
            (199_999, 1),
            (200_000, 0),
            (0, 200_000),
            (12_345, 54_321),
        ] {
            let mut buf = vec![0u8; len];
            src.read_at(offset as u64, &mut buf).unwrap();
            assert!(buf == data[offset..offset + len], "{offset} {len}");
        }
        let mut buf = vec![0u8; 4096];
        src.read_bulk(1000, &mut buf).unwrap();
        assert!(buf == data[1000..5096]);
    }

    #[test]
    fn concurrent_reads() {
        let data = pattern(300_000);
        let tmp = temp_file(&data);
        let src = FileSource::open(&tmp.0).unwrap();
        std::thread::scope(|scope| {
            for t in 0..4usize {
                let (src, data) = (&src, &data);
                scope.spawn(move || {
                    for i in 0..50usize {
                        let offset = (t * 70_001 + i * 3_001) % 250_000;
                        let mut buf = vec![0u8; 4_000 + i];
                        src.read_at(offset as u64, &mut buf).unwrap();
                        assert!(buf == data[offset..offset + buf.len()]);
                    }
                });
            }
        });
    }

    #[test]
    fn out_of_bounds_fails() {
        let tmp = temp_file(&pattern(1000));
        let src = FileSource::open(&tmp.0).unwrap();
        for (offset, len) in [
            (1000u64, 1usize),
            (1001, 0),
            (999, 2),
            (0, 1001),
            (u64::MAX, 1),
        ] {
            let mut buf = vec![0xAAu8; len];
            let err = src.read_at(offset, &mut buf).unwrap_err();
            assert!(matches!(err, Error::OutOfBounds { .. }), "{err:?}");
            assert!(buf.iter().all(|&b| b == 0xAA));
        }
    }

    #[test]
    fn empty_file() {
        let tmp = temp_file(&[]);
        let src = FileSource::open(&tmp.0).unwrap();
        assert!(src.is_empty());
        src.read_at(0, &mut []).unwrap();
        assert!(src.read_at(0, &mut [0u8; 1]).is_err());
    }

    #[test]
    fn missing_path_fails() {
        let path = temp_path();
        let err = FileSource::open(&path).err().unwrap();
        match err {
            Error::Io {
                context: IoContext::Path { op, path: p },
                source,
            } => {
                assert_eq!(op, "opening");
                assert_eq!(p, path);
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected Io opening error, got {other:?}"),
        }
    }

    #[test]
    fn directory_fails() {
        let dir = std::env::temp_dir();
        let err = FileSource::open(&dir).err().unwrap();
        match err {
            Error::Io {
                context: IoContext::Path { op, .. },
                source,
            } => {
                assert_eq!(op, "opening");
                assert_eq!(source.kind(), io::ErrorKind::IsADirectory);
            }
            other => panic!("expected Io opening error, got {other:?}"),
        }
    }

    #[test]
    fn shrunk_file_fails_instead_of_short_read() {
        let tmp = temp_file(&pattern(10_000));
        let src = FileSource::open(&tmp.0).unwrap();
        std::fs::write(&tmp.0, pattern(100)).unwrap();
        let mut buf = vec![0u8; 1000];
        let err = src.read_at(5000, &mut buf).unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        let mut buf = vec![0u8; 1000];
        let err = src.read_at(0, &mut buf).unwrap_err();
        match err {
            Error::Io { context, source } => {
                assert_eq!(
                    context,
                    IoContext::Read {
                        offset: 0,
                        len: 1000
                    }
                );
                assert_eq!(source.kind(), io::ErrorKind::UnexpectedEof);
            }
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }
}
