//! Unix integration shared by applications and the daemon.
//! The current implementations require Linux.
pub mod identity;
pub mod ipc;
pub mod machine;
pub mod network;
pub mod paths;
pub mod storage;
