//! # Module overview
//! Gateway orchestration and shard lifecycle helpers.
//!
//! ## Data flow
//! `restore` seeds shard configs, `run` dispatches events, and `save` persists
//! session resume info on shutdown.

pub mod dispatch;
pub mod event;
pub mod resume;
