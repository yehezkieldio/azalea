//! Dev-only JSON schema generator for taplo/TOML tooling.
//!
//! ## Usage
//! `generate-schema [output-path|-]` writes the schema to a file or stdout.
//!
//! ## References
//! - Taplo schema format: <https://taplo.tamasfe.dev/configuration/schema.html>
#![allow(unused_crate_dependencies)]

#[path = "../ids.rs"]
#[allow(dead_code)]
mod ids;

#[path = "../config.rs"]
#[allow(dead_code)]
mod config;

use anyhow::Result;
use std::fs::File;
use std::io::{self, Write};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let output = args
        .next()
        .unwrap_or_else(|| "azalea.schema.json".to_string());

    if args.next().is_some() {
        anyhow::bail!("usage: generate-schema [output-path|-]");
    }

    let schema = schemars::schema_for!(config::AppConfig);

    if output == "-" {
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        serde_json::to_writer_pretty(&mut handle, &schema)?;
        writeln!(handle)?;
    } else {
        let file = File::create(&output)?;
        serde_json::to_writer_pretty(file, &schema)?;
    }

    Ok(())
}
