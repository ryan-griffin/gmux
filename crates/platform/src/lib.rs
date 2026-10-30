//! Shared platform services for Gmux, without a terminal engine or GUI.
//! Portable helpers are separate from the current Linux implementations.
pub mod paths;
pub mod tcp;
#[cfg(target_os = "linux")]
pub mod unix;
