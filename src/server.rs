//! The MCP server: holds the combined tool router and serves the handler info.
//! Tool methods themselves live in `tools::device` and `tools::monitor`, each
//! contributing a named router that is merged here.

use rmcp::{
    ServerHandler,
    handler::server::tool::ToolRouter,
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    tool_handler,
};

#[derive(Clone)]
pub struct EspflashServer {
    tool_router: ToolRouter<Self>,
}

impl EspflashServer {
    pub fn new() -> Self {
        Self {
            // Combine the per-module routers (see tools::device / tools::monitor).
            tool_router: Self::device_router() + Self::capture_router(),
        }
    }
}

#[tool_handler]
impl ServerHandler for EspflashServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: env!("CARGO_PKG_NAME").to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: Some(
                "ESP device flash tool. Connect to ESP32/ESP32-S2/S3/C2/C3/C5/C6/H2/P4 \
                 devices via serial port for flashing, erasing, and reading flash memory.\n\n\
                 ## Tools\n\
                 - list_ports: Discover available serial ports\n\
                 - chip_info: Get device type, revision, MAC, flash size\n\
                 - flash: Flash ELF or raw binary firmware\n\
                 - flash_monitor: Flash firmware then capture boot serial output\n\
                 - rerun: Reset the device and monitor the fresh boot (no reflash)\n\
                 - monitor: Read serial output with timeout/pattern stop\n\
                 - erase_flash: Erase entire flash (destructive)\n\
                 - erase_region: Erase specific flash region (destructive)\n\
                 - read_flash: Read flash contents to file\n\
                 - reset_device: Hardware reset the device\n\
                 - checksum_md5: Compute MD5 of flash region\n\n\
                 All tools that communicate with a device require a serial port path. \
                 Use list_ports first to discover available ports.\n\n\
                 ## Monitoring\n\
                 monitor, flash_monitor, and rerun stop when: max timeout reached, \
                 a regex `stop` matches (substring/unanchored, alternation OK), \
                 idle_ms passes with no new data, or the byte cap is reached. \
                 By default they strip ROM baud-mismatch garbage + ESP-IDF bootloader \
                 log lines (strip_boot_noise) and ANSI color codes (strip_ansi), and on \
                 a `stop` match they focus output on the matched line (set \
                 context for N preceding lines). Defaults: 5s timeout, 4s idle.\n\n\
                 For iterating on the same firmware, prefer rerun over reset_device + \
                 monitor. flash_monitor captures from boot (it does not flush)."
                    .into(),
            ),
        }
    }
}
