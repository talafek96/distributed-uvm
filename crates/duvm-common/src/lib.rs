//! duvm-common: Shared types and ring buffer protocol for the duvm system.
//!
//! This crate defines the core data structures shared between the kernel module
//! (via C FFI) and the user-space daemon, including the lock-free ring buffer
//! protocol, page handle types, and request/response formats.

pub mod page;
pub mod protocol;
pub mod ring;
pub mod stats;
