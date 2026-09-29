//! CLI output requires a subprocess, with an isolated home and empty env.
#![cfg(unix)]
#[path = "inline/monitor_cli_binary.rs"]
mod tests;
