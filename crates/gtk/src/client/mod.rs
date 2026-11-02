//! GTK workspace organization, presentation, and execution adapters.
pub mod backend;
pub mod connections;
pub mod control;
pub mod ipc;
pub mod model;
pub mod request;
mod store;
pub mod window;
pub use store::{Snapshot, Store, Subscription};
