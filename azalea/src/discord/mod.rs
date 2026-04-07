//! # Module overview
//! Discord command handling and message response helpers.
//!
//! ## Data flow
//! `commands` registers slash commands at startup, and `responder` updates
//! progress/error messages during pipeline execution.

pub mod commands;
mod responder;

pub use commands::{handle_interaction, register};
pub use responder::{
    cleanup_interaction_response, cleanup_processing, delete_original, send_error,
    send_interaction_error, send_processing, spawn_progress_updates,
};
