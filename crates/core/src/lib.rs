pub mod capture;
pub mod clock;
pub mod crypto;
pub mod input;
pub mod ipc;
pub mod manager;
pub mod model;
pub mod parser;
pub mod presentation;
mod process;
pub mod store;

pub use manager::RecorderManager;
pub use model::*;
