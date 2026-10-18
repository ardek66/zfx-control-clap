//! Re-export of the shared IPC protocol crate.
//!
//! Kept as a module path so existing `crate::protocol::` / `protocol::` call
//! sites remain stable.

pub use zfx_protocol::*;
