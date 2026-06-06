//! Shared helpers for the rev benchmark binaries.
//!
//! Each `src/bin/*.rs` is a self-contained hyperfine subject: parses an
//! iteration count, does its work, prints a one-line result. This module
//! hosts the WireBus frame codec + a tiny timing helper so the bench
//! binaries stay focused on what they're measuring.

pub mod wirebus;
pub mod timing;
