//! Cache abstractions for Hurry.
//!
//! This module provides a unified caching layer that supports both:
//! - **Remote caching** via Courier HTTP API (for distributed teams)
//! - **Local caching** via filesystem + SQLite (for solo developers)
//!
//! The main abstraction is the [`CacheBackend`] trait, with two implementations:
//! - [`CourierBackend`]: Uses remote Courier server
//! - [`LocalBackend`]: Uses local filesystem + SQLite

mod backend;
mod courier_backend;
pub mod local;

pub use backend::{BulkStoreResult, CacheBackend};
pub use courier_backend::CourierBackend;
pub use local::LocalBackend;
