//! The MCP server: holds the combined tool router and serves the handler info.
//! Tool methods themselves live in `tools::device` and `tools::monitor`, each
//! contributing a named router that is merged here.

use rmcp::{
    ServerHandler,
    handler::server::tool::ToolRouter,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool_handler,
};

#[derive(Clone)]
pub struct Server {
    tool_router: ToolRouter<Self>,
}

impl Server {
    pub fn new() -> Self {
        Self {
            // Combine the per-module routers (see tools::device / tools::monitor).
            tool_router: Self::device_router() + Self::capture_router(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    // The protocol version is left at rmcp's default (the newest revision the
    // crate implements). rmcp negotiates down to `min(client, server)` on
    // initialize, so advertising the newest is what lets a modern client use
    // modern features; pinning an old one would cap *every* client to it.
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Flash + monitor embedded targets over two backends — probe-rs \
                 (JTAG/SWD + RTT; any probe-rs target: STM32, nRF, RP2350, ESP \
                 Xtensa+RISC-V, …) and espflash (UART; ESP only).\n\n\
                 ## Backend (required)\n\
                 Every flash/monitor call needs `backend`:\n\
                 - \"probe-rs\": JTAG/SWD flashing + RTT capture. Use for firmware that \
                   logs over RTT (defmt-rtt / rtt-target), and for all non-ESP chips.\n\
                 - \"espflash\": UART flashing + serial capture. Use for firmware that \
                   logs over UART (esp-println). ESP only.\n\
                 Both work on ESP chips; pick the one matching where the firmware emits \
                 output (else you'll see no logs). It is not auto-detected.\n\n\
                 ## Auto-detection (override any per call)\n\
                 - elf / file to flash: the build artifact, via `cargo metadata` (pass \
                   `project_dir` if the server's cwd isn't the project; `bin` for \
                   multi-binary workspaces).\n\
                 - chip (probe-rs): from .cargo/config.toml's runner --chip.\n\
                 - port (espflash): the sole USB serial port.\n\
                 - defmt vs text: from the ELF's `.defmt` section (reliable).\n\n\
                 ## Tools\n\
                 - flash: flash firmware (no monitor)\n\
                 - flash_monitor: flash, then capture from boot\n\
                 - rerun: reset + capture (no reflash); repeat>1 for flaky-bug runs\n\
                 - monitor: attach + capture only\n\
                 - chip_info / erase_flash / erase_region / read_flash / reset_device / \
                   checksum_md5 / list_ports (espflash/serial)\n\n\
                 ## Capture\n\
                 Stops on: `stop` regex match, `stop_on_level` (defmt), idle_ms, max \
                 timeout, or byte cap. Provide an ELF (or let it auto-detect) to decode \
                 defmt — then `level`/`module` filter structurally and a suppressed \
                 count reports what was hidden. Text mode strips boot noise + ANSI.\n\n\
                 ## Sending to the target\n\
                 monitor / flash_monitor / rerun take `send`: a string written to the \
                 device before reading (probe-rs: RTT down-channel 0; espflash: serial \
                 TX), so the reply is captured in the same call. Escapes `\\n` `\\r` \
                 `\\t` `\\0` `\\xNN` are interpreted — line-based firmware usually needs \
                 a trailing `\\n`. Pair with `stop` to return as soon as the answer \
                 arrives, and `send_delay_ms` to let a just-reset device boot first.\n\
                 probe-rs requires the firmware to declare an RTT down-channel: \
                 `defmt-rtt` does not (max_down_channels = 0), so input needs \
                 `rtt-target` (defmt still works there via `set_defmt_channel`).",
            )
    }
}
