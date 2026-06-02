//! MCP tool implementations, split by role into two routers combined in
//! `server.rs`: `device_router` (device ops) and `capture_router` (monitoring).

pub mod device;
pub mod monitor;
