pub mod hotwords;
mod mcp;
mod paths;
mod tools;

pub use mcp::run_stdio_server;
pub use paths::{shared_data_dir, SHARED_DIRECTORY_NAME};
pub use tools::AgentTools;
