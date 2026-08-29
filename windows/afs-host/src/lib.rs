//! AFS — Android file service client.
//!
//! Split into a library plus a thin binary on purpose. Most of this crate exists
//! to be called by the WinFsp callbacks in [`winfsp_glue`], which is still a stub,
//! so in a binary-only crate every one of these items reads as dead code. As a
//! library they are public API, which is what they actually are: `proto` and
//! `client` are a complete, transport-agnostic implementation of the protocol in
//! `docs/protocol.md`, usable by anything — the mount host, integration tests, or
//! a future GUI.
//!
//! Layering, bottom up:
//!
//! - [`proto`] — wire codec. Pure std, no I/O policy, mirrors the Kotlin side.
//! - [`client`] — one connection, many concurrent requests, matched by tag.
//! - [`fs`] — AFS semantics to Windows semantics. Pure and unit-tested.
//! - [`adb`] — locating adb and managing the USB port forward.
//! - [`winfsp_glue`] — the WinFsp binding and the mount loop.

pub mod adb;
pub mod client;
pub mod fs;
pub mod proto;
pub mod stats;
pub mod winfsp_glue;
