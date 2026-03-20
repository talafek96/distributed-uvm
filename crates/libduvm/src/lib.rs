//! libduvm: User-space library for duvm distributed memory.
//!
//! Provides both a Rust API and a C FFI for applications that want fine-grained
//! control over distributed memory (prefetch, tier placement, etc.).
//!
//! For unmodified applications, duvm works transparently via the kernel module
//! or userfaultfd. This library is for applications that want to optimize.

pub mod ffi;
pub mod pool;

pub use pool::Pool;
