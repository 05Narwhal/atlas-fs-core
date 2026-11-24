// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (C) 2026 Julian A. Loy.

//! Core traits and types for reading filesystems and disk images.
//!
//! [`io::block::BlockSource`] is a positional, thread-safe reader over a
//! device, an image or a slice of either. [`fs::FileSystem`] is a read-only
//! tree of nodes that a format crate builds on top of a `BlockSource`.
//! [`error::Error`] is the one error type every reader returns: on-disk
//! bytes are untrusted, so a reader reports damage as an error and never
//! panics.
//!
//! ```
//! use atlas_fs_core::io::block::{BlockSource, MemSource};
//!
//! let source = MemSource::new(vec![0u8, 1, 2, 3, 4, 5, 6, 7]);
//! assert_eq!(source.len(), 8);
//! let mut buf = [0u8; 4];
//! source.read_at(2, &mut buf).unwrap();
//! assert_eq!(buf, [2, 3, 4, 5]);
//! assert!(source.read_at(6, &mut buf).is_err());
//! ```

#![forbid(unsafe_code)]
// On-disk bytes are hostile. These lints make the usual ways of trusting
// them (unchecked indexing, arithmetic and casts, panicking shortcuts) fail
// the clippy gate. Unit tests are exempt.
#![warn(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used
)]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod error;
pub mod fs;
pub mod io;
