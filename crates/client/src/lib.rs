//! Daemon connections, retries, terminal caches, and bounded input/output queues.
//! Linux is currently supported; Platform helpers live in `unix` and `gmux-platform`.
pub mod connection;
pub mod connections;
mod incoming;
mod io;
pub mod ipc;
pub mod local;
pub mod network;
pub mod remote;
pub mod runtime;
mod scroll_cache;
pub mod unix;
