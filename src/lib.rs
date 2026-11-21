//! `hoo-browser` library: the tier C helper's building blocks.
//!
//! The binary (`src/main.rs`) wires these together; integration tests drive
//! `server::Helper` in-process with a fake agent-browser.

pub mod backend;
pub mod client;
pub mod error;
pub mod exec;
pub mod server;
pub mod state;
