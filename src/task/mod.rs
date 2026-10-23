//! Task model and lifecycle.
//!
//! Sub-modules:
//! - [`registry`]: in-memory + disk-mirrored task records with terminal-event
//!   idempotence, orphan reaping, and elastic budget extension.
//! - [`semaphore`]: cross-process file-lease slot semaphore with tenant
//!   exclusivity, FIFO queuing, and stale-lease reclaim.

pub mod registry;
pub mod semaphore;
pub mod trace;
pub mod wait;
