//! Concurrency controls shared across the pipeline.
//!
//! ## Design rationale
//! Centralizing semaphores keeps parallelism policy consistent across resolver,
//! downloader, and transcode stages.
//!
//! ## Invariants
//! - All permit counts are at least one (see [`permits::Permits::new`]).
//! - Callers treat permits as shared, cloneable handles.
//!
//! ## Concurrency assumptions
//! These semaphores are expected to be held across await points; they must be
//! cloned with `Arc` semantics and are safe to share across tasks.

pub mod permits;

pub use permits::Permits;
