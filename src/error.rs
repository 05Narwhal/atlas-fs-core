// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! Engine error types.

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::PathBuf;

/// Result alias used across the engine.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every way an engine operation can fail.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The device, image or host file could not be read.
    #[error("{context}: {source}")]
    Io {
        /// What the engine was doing.
        context: IoContext,
        /// The error the operating system returned.
        #[source]
        source: io::Error,
    },

    /// The operating system refused access. The app shows "needs elevation"
    /// for a device and "no permission" for a folder.
    #[error("{context}: access denied")]
    AccessDenied {
        /// What the engine was doing.
        context: IoContext,
    },

    /// A read was requested outside the source.
    #[error(
        "read of {len} bytes at byte offset {offset} is past the end of the source ({size} bytes)"
    )]
    OutOfBounds {
        /// Byte offset of the requested read.
        offset: u64,
        /// Length of the requested read in bytes.
        len: u64,
        /// Size of the source in bytes.
        size: u64,
    },

    /// An on-disk structure is damaged or is not what it claims to be.
    #[error(transparent)]
    Corrupt(#[from] Corruption),

    /// The format, or a feature of it, is recognised but not supported yet.
    #[error("not supported: {0}")]
    Unsupported(Cow<'static, str>),

    /// The node id does not exist in this filesystem.
    #[error("node {node} does not exist")]
    NotFound {
        /// The node id that was looked up.
        node: u64,
    },

    /// The node was expected to be a folder and is not.
    #[error("node {node} is not a folder")]
    NotADirectory {
        /// The node id that was used as a folder.
        node: u64,
    },

    /// The node was expected to be a file and is a folder.
    #[error("node {node} is a folder")]
    IsADirectory {
        /// The node id that was used as a file.
        node: u64,
    },

    /// The node has no data stream of the requested kind.
    #[error("node {node} has no such stream")]
    NoSuchStream {
        /// The node id that was asked for a stream.
        node: u64,
    },

    /// A directory cursor was used with a folder it does not belong to.
    #[error("the directory cursor is not valid for this folder")]
    InvalidCursor,

    /// Eject or dismount was refused because another program holds the
    /// volume. `holders` names up to 8 of them when the OS says; empty means
    /// it did not ("in use by another program").
    #[error("in use by another program")]
    InUse {
        /// Process names, at most 8, lossily decoded and length-bounded.
        holders: Vec<String>,
    },

    /// The password given to `LockedVolume::unlock` opens no key on the
    /// volume. Carries nothing: neither the password nor which key was
    /// tried belongs in a log.
    #[error("the password did not unlock the volume")]
    WrongPassword,

    /// An archive entry is encrypted and no password was available: none is
    /// configured in the archive options, or the callback declined to give one.
    /// The caller asks the person and retries.
    #[error("a password is required")]
    PasswordRequired,

    /// A long operation (extracting an archive entry) was stopped because the
    /// caller's progress callback returned `false`.
    #[error("cancelled")]
    Cancelled,
}

impl Error {
    /// Wraps an OS error. `io::ErrorKind::PermissionDenied` becomes
    /// `Error::AccessDenied`; everything else becomes `Error::Io`.
    pub fn io(context: IoContext, source: io::Error) -> Self {
        if source.kind() == io::ErrorKind::PermissionDenied {
            Self::AccessDenied { context }
        } else {
            Self::Io { context, source }
        }
    }

    /// Builds an `Error::Unsupported` from a static or owned description.
    pub fn unsupported(what: impl Into<Cow<'static, str>>) -> Self {
        Self::Unsupported(what.into())
    }
}

/// What the engine was doing when the operating system returned an error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IoContext {
    /// A positional read from a block source.
    Read {
        /// Byte offset of the read.
        offset: u64,
        /// Length of the read in bytes.
        len: u64,
    },
    /// An operation on a host path. `op` is a gerund: "opening", "listing", "reading".
    Path {
        /// The operation, as a gerund.
        op: &'static str,
        /// The host path the operation was applied to.
        path: PathBuf,
    },
}

impl fmt::Display for IoContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { offset, len } => {
                write!(f, "reading {len} bytes at byte offset {offset}")
            }
            Self::Path { op, path } => write!(f, "{op} {}", path.display()),
        }
    }
}

/// A damaged on-disk structure: what was being read, where, and what was wrong.
///
/// Displays as `"{structure} {location}: {kind}"`, or `"{structure}: {kind}"`
/// when the location is [`Location::Unspecified`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corruption {
    /// The structure being read, such as "catalog B-tree".
    pub structure: &'static str,
    /// Where in the structure the problem was found.
    pub location: Location,
    /// What was wrong.
    pub kind: CorruptKind,
}

impl Corruption {
    /// Builds a `Corruption` without allocating.
    pub const fn new(structure: &'static str, location: Location, kind: CorruptKind) -> Self {
        Self {
            structure,
            location,
            kind,
        }
    }
}

impl fmt::Display for Corruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.location {
            Location::Unspecified => write!(f, "{}: {}", self.structure, self.kind),
            location => write!(f, "{} {}: {}", self.structure, location, self.kind),
        }
    }
}

impl std::error::Error for Corruption {}

/// Where in a structure a problem was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Location {
    /// No useful location; displays as nothing.
    Unspecified,
    /// A byte offset into the source.
    Byte(u64),
    /// A filesystem block number.
    Block(u64),
    /// A B-tree node number.
    Node(u32),
    /// A record within a B-tree node.
    Record {
        /// The B-tree node number.
        node: u32,
        /// The record index within the node.
        record: u16,
    },
    /// An object identifier.
    Object(u64),
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unspecified => Ok(()),
            Self::Byte(offset) => write!(f, "byte offset {offset}"),
            Self::Block(block) => write!(f, "block {block}"),
            Self::Node(node) => write!(f, "node {node}"),
            Self::Record { node, record } => write!(f, "node {node} record {record}"),
            Self::Object(object) => write!(f, "object {object}"),
        }
    }
}

/// What was wrong with a damaged structure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptKind {
    /// A magic number or signature did not match.
    BadSignature {
        /// The signature the format requires.
        expected: u64,
        /// The signature found on disk.
        found: u64,
    },
    /// Fewer bytes were available than the structure needs.
    Truncated {
        /// Bytes the structure needs.
        needed: u64,
        /// Bytes actually available.
        available: u64,
    },
    /// A field points or counts past a limit.
    OutOfRange {
        /// The field that was read.
        field: &'static str,
        /// The value read from disk.
        value: u64,
        /// Name of the limit the value was checked against.
        limit_name: &'static str,
        /// The limit.
        limit: u64,
    },
    /// A field holds a value the format does not allow.
    InvalidValue {
        /// The field that was read.
        field: &'static str,
        /// The value read from disk.
        value: u64,
    },
    /// Arithmetic on a field overflowed.
    Overflow {
        /// The field whose arithmetic overflowed.
        field: &'static str,
    },
    /// A linked structure loops back on itself.
    Loop {
        /// The structure that loops.
        what: &'static str,
    },
    /// A traversal exceeded its safety bound.
    LimitExceeded {
        /// What exceeded the bound.
        what: &'static str,
        /// The bound.
        limit: u64,
    },
    /// Any other damage, described by the text itself.
    Other(&'static str),
}

impl fmt::Display for CorruptKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadSignature { expected, found } => {
                write!(f, "bad signature: expected {expected:#x}, found {found:#x}")
            }
            Self::Truncated { needed, available } => {
                write!(f, "needs {needed} bytes, only {available} available")
            }
            Self::OutOfRange {
                field,
                value,
                limit_name,
                limit,
            } => write!(f, "{field} {value} past {limit_name} {limit}"),
            Self::InvalidValue { field, value } => {
                write!(f, "{field} has invalid value {value}")
            }
            Self::Overflow { field } => write!(f, "{field} overflows"),
            Self::Loop { what } => write!(f, "{what} forms a loop"),
            Self::LimitExceeded { what, limit } => {
                write!(f, "{what} exceeds the limit of {limit}")
            }
            Self::Other(text) => f.write_str(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    fn corrupt(location: Location, kind: CorruptKind) -> String {
        Corruption::new("structure", location, kind).to_string()
    }

    #[test]
    fn architecture_example() {
        let c = Corruption::new(
            "catalog B-tree",
            Location::Node(4127),
            CorruptKind::OutOfRange {
                field: "record offset",
                value: 9000,
                limit_name: "node size",
                limit: 8192,
            },
        );
        assert_eq!(
            c.to_string(),
            "catalog B-tree node 4127: record offset 9000 past node size 8192"
        );
    }

    #[test]
    fn unspecified_location_has_no_stray_space() {
        let c = Corruption::new(
            "volume header",
            Location::Unspecified,
            CorruptKind::BadSignature {
                expected: 0x482b,
                found: 0,
            },
        );
        assert_eq!(
            c.to_string(),
            "volume header: bad signature: expected 0x482b, found 0x0"
        );
    }

    #[test]
    fn every_corrupt_kind() {
        let loc = Location::Unspecified;
        assert_eq!(
            corrupt(
                loc,
                CorruptKind::BadSignature {
                    expected: 0x482b,
                    found: 0
                }
            ),
            "structure: bad signature: expected 0x482b, found 0x0"
        );
        assert_eq!(
            corrupt(
                loc,
                CorruptKind::Truncated {
                    needed: 512,
                    available: 100
                }
            ),
            "structure: needs 512 bytes, only 100 available"
        );
        assert_eq!(
            corrupt(
                loc,
                CorruptKind::OutOfRange {
                    field: "record offset",
                    value: 9000,
                    limit_name: "node size",
                    limit: 8192
                }
            ),
            "structure: record offset 9000 past node size 8192"
        );
        assert_eq!(
            corrupt(
                loc,
                CorruptKind::InvalidValue {
                    field: "node kind",
                    value: 9
                }
            ),
            "structure: node kind has invalid value 9"
        );
        assert_eq!(
            corrupt(
                loc,
                CorruptKind::Overflow {
                    field: "extent end"
                }
            ),
            "structure: extent end overflows"
        );
        assert_eq!(
            corrupt(loc, CorruptKind::Loop { what: "leaf chain" }),
            "structure: leaf chain forms a loop"
        );
        assert_eq!(
            corrupt(
                loc,
                CorruptKind::LimitExceeded {
                    what: "tree depth",
                    limit: 16
                }
            ),
            "structure: tree depth exceeds the limit of 16"
        );
        assert_eq!(
            corrupt(loc, CorruptKind::Other("free-form text")),
            "structure: free-form text"
        );
    }

    #[test]
    fn every_location() {
        let kind = || CorruptKind::Other("bad");
        assert_eq!(corrupt(Location::Unspecified, kind()), "structure: bad");
        assert_eq!(
            corrupt(Location::Byte(9000), kind()),
            "structure byte offset 9000: bad"
        );
        assert_eq!(
            corrupt(Location::Block(12), kind()),
            "structure block 12: bad"
        );
        assert_eq!(
            corrupt(Location::Node(4127), kind()),
            "structure node 4127: bad"
        );
        assert_eq!(
            corrupt(
                Location::Record {
                    node: 4127,
                    record: 3
                },
                kind()
            ),
            "structure node 4127 record 3: bad"
        );
        assert_eq!(
            corrupt(Location::Object(77), kind()),
            "structure object 77: bad"
        );
    }

    #[test]
    fn io_context_display() {
        assert_eq!(
            IoContext::Read {
                offset: 4096,
                len: 512
            }
            .to_string(),
            "reading 512 bytes at byte offset 4096"
        );
        let path = PathBuf::from("some").join("file.img");
        assert_eq!(
            IoContext::Path {
                op: "opening",
                path: path.clone()
            }
            .to_string(),
            format!("opening {}", path.display())
        );
    }

    #[test]
    fn io_permission_denied_becomes_access_denied() {
        let context = IoContext::Read {
            offset: 0,
            len: 512,
        };
        let err = Error::io(
            context.clone(),
            io::Error::from(io::ErrorKind::PermissionDenied),
        );
        match &err {
            Error::AccessDenied { context: c } => assert_eq!(c, &context),
            other => panic!("expected AccessDenied, got {other:?}"),
        }
        assert_eq!(
            err.to_string(),
            "reading 512 bytes at byte offset 0: access denied"
        );
    }

    #[test]
    fn io_other_kinds_keep_their_source() {
        let context = IoContext::Read { offset: 8, len: 16 };
        let err = Error::io(context, io::Error::new(io::ErrorKind::NotFound, "gone"));
        assert!(matches!(err, Error::Io { .. }));
        assert!(err.source().is_some());
        assert_eq!(err.to_string(), "reading 16 bytes at byte offset 8: gone");
    }

    #[test]
    fn every_error_variant() {
        assert_eq!(
            Error::OutOfBounds {
                offset: 100,
                len: 50,
                size: 120
            }
            .to_string(),
            "read of 50 bytes at byte offset 100 is past the end of the source (120 bytes)"
        );
        assert_eq!(
            Error::unsupported("compressed forks").to_string(),
            "not supported: compressed forks"
        );
        assert_eq!(
            Error::unsupported(String::from("owned text")).to_string(),
            "not supported: owned text"
        );
        assert_eq!(
            Error::NotFound { node: 5 }.to_string(),
            "node 5 does not exist"
        );
        assert_eq!(
            Error::NotADirectory { node: 6 }.to_string(),
            "node 6 is not a folder"
        );
        assert_eq!(
            Error::IsADirectory { node: 7 }.to_string(),
            "node 7 is a folder"
        );
        assert_eq!(
            Error::NoSuchStream { node: 8 }.to_string(),
            "node 8 has no such stream"
        );
        assert_eq!(
            Error::InvalidCursor.to_string(),
            "the directory cursor is not valid for this folder"
        );
        assert_eq!(
            Error::WrongPassword.to_string(),
            "the password did not unlock the volume"
        );
        let io_err = Error::Io {
            context: IoContext::Path {
                op: "listing",
                path: PathBuf::from("dir"),
            },
            source: io::Error::other("boom"),
        };
        assert_eq!(io_err.to_string(), "listing dir: boom");
        let denied = Error::AccessDenied {
            context: IoContext::Path {
                op: "opening",
                path: PathBuf::from("dev"),
            },
        };
        assert_eq!(denied.to_string(), "opening dev: access denied");
    }

    #[test]
    fn corrupt_conversion_keeps_text() {
        let c = Corruption::new(
            "catalog B-tree",
            Location::Record { node: 1, record: 2 },
            CorruptKind::Loop { what: "leaf chain" },
        );
        let text = c.to_string();
        let err = Error::from(c);
        assert!(matches!(err, Error::Corrupt(_)));
        assert_eq!(err.to_string(), text);
    }

    #[test]
    fn error_is_send_sync_static() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<Error>();
    }
}
