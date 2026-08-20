pub mod app;
pub mod backend;
pub mod cli;
pub mod config;
pub mod discovery;
pub mod domain;
pub mod error;
pub mod logging;
pub mod runtime;
pub mod terminal;
#[cfg(any(test, feature = "simulator"))]
pub mod test_support;
pub mod tui;
