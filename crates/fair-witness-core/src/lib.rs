//! Watching a research project folder and reviewing its changes against the project's DMP,
//! without a terminal interface.

pub mod auth;
pub mod damap;
pub mod net;
pub mod project;
pub mod review;
pub mod secret;
pub mod session;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod watcher;
