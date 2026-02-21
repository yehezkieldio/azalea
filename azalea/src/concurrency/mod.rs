//! # Module overview
//! Application-specific concurrency helpers.
//!
//! ## Data flow
//! The rate limiter is used by [`crate::gateway::event::handle`] to throttle
//! message-triggered pipeline work.

pub mod ratelimit;

pub use ratelimit::RateLimiter;
