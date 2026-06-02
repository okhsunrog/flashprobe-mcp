//! Device-operation tools (espflash backend): port discovery, chip info,
//! flashing, erasing, reading, reset, and checksums. Grouped into the
//! `device_router`, combined with the capture tools in `server.rs`.

use crate::backend::espflash::{connect_to_device, flash_file};
use crate::inputs::*;
use crate::server::EspflashServer;
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use serialport::SerialPortType;

#[tool_router(router = device_router, vis = "pub(crate)")]
impl EspflashServer {
    #[tool(description = "List available serial ports that could have ESP devices attached")]
    async fn list_ports(
        &self,
        Parameters(_input): Parameters<ListPortsInput>,
    ) -> Result<CallToolResult, McpError> {
        let ports = tokio::task::spawn_blocking(serialport::available_ports)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if ports.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No serial ports found.",
            )]));
        }

        let mut output = format!("Found {} serial port(s):\n\n", ports.len());
        for port in &ports {
            output.push_str(&format!("- {}", port.port_name));
            match &port.port_type {
                SerialPortType::UsbPort(info) => {
                    output.push_str(&format!(" [USB {:04x}:{:04x}", info.vid, info.pid));
                    if let Some(product) = &info.product {
                        output.push_str(&format!(" {product}"));
                    }
                    if let Some(manufacturer) = &info.manufacturer {
                        output.push_str(&format!(" ({manufacturer})"));
                    }
                    output.push(']');
                }
                SerialPortType::PciPort => output.push_str(" [PCI]"),
                _ => {}
            }
            output.push('\n');
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        description = "Connect to an ESP device and retrieve chip information including type, revision, MAC address, flash size, and features"
    )]
    async fn chip_info(
        &self,
        Parameters(input): Parameters<ChipInfoInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            let info = flasher
                .device_info()
                .map_err(|e| format!("Failed to get device info: {e}"))?;

            let mut output = String::from("## Device Information\n\n");
            output.push_str(&format!("- Chip: {}\n", info.chip));
            if let Some((major, minor)) = info.revision {
                output.push_str(&format!("- Revision: v{major}.{minor}\n"));
            }
            output.push_str(&format!("- Crystal frequency: {}\n", info.crystal_frequency));
            output.push_str(&format!("- Flash size: {}\n", info.flash_size));
            if let Some(mac) = &info.mac_address {
                output.push_str(&format!("- MAC address: {mac}\n"));
            }
            if !info.features.is_empty() {
                output.push_str(&format!("- Features: {}\n", info.features.join(", ")));
            }

            Ok(output)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Flash an ELF or raw binary file to an ESP device. For ELF files, the IDF bootloader format is used automatically. For raw binaries, provide a flash_address."
    )]
    async fn flash(
        &self,
        Parameters(input): Parameters<FlashInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let file_data = std::fs::read(&input.file_path)
                .map_err(|e| format!("Failed to read file '{}': {e}", input.file_path))?;

            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            let summary = flash_file(
                &mut flasher,
                &file_data,
                input.flash_address,
                input.partition_table.as_deref(),
                input.bootloader.as_deref(),
            )?;
            Ok(format!("Successfully {}", lower_first(&summary)))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Erase the entire flash memory of the connected ESP device. WARNING: This is irreversible and will delete all data including firmware."
    )]
    async fn erase_flash(
        &self,
        Parameters(input): Parameters<EraseFlashInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            flasher
                .erase_flash()
                .map_err(|e| format!("Failed to erase flash: {e}"))?;

            Ok("Successfully erased entire flash memory.".to_string())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(
        description = "Erase a specific region of flash memory. Both address and size must be 4096-byte (0x1000) aligned."
    )]
    async fn erase_region(
        &self,
        Parameters(input): Parameters<EraseRegionInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            if input.address % 0x1000 != 0 {
                return Err(format!(
                    "Address 0x{:08x} is not 4096-byte aligned",
                    input.address
                ));
            }
            if input.size % 0x1000 != 0 {
                return Err(format!("Size 0x{:x} is not 4096-byte aligned", input.size));
            }

            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            flasher
                .erase_region(input.address, input.size)
                .map_err(|e| format!("Failed to erase region: {e}"))?;

            Ok(format!(
                "Successfully erased 0x{:x} bytes at address 0x{:08x}",
                input.size, input.address
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(description = "Read flash memory contents and save to a file")]
    async fn read_flash(
        &self,
        Parameters(input): Parameters<ReadFlashInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            let output_path = std::path::PathBuf::from(&input.output_path);

            flasher
                .read_flash(
                    input.address,
                    input.size,
                    0x400, // block_size
                    32,    // max_in_flight
                    output_path.clone(),
                )
                .map_err(|e| format!("Failed to read flash: {e}"))?;

            let file_size = std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);

            Ok(format!(
                "Successfully read 0x{:x} bytes from address 0x{:08x} to '{}' ({file_size} bytes written)",
                input.size,
                input.address,
                output_path.display()
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(description = "Reset the connected ESP device using DTR/RTS serial control lines")]
    async fn reset_device(
        &self,
        Parameters(input): Parameters<ResetDeviceInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            // Connect without stub for reset (matches espflash CLI behavior)
            let mut flasher = connect_to_device(&input.port, 115_200, false)?;

            flasher
                .connection()
                .reset()
                .map_err(|e| format!("Failed to reset device: {e}"))?;

            Ok("Device reset successfully.".to_string())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(description = "Compute the MD5 checksum of a flash memory region")]
    async fn checksum_md5(
        &self,
        Parameters(input): Parameters<ChecksumMd5Input>,
    ) -> Result<CallToolResult, McpError> {
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut flasher = connect_to_device(&input.port, input.baud, true)?;

            let checksum = flasher
                .checksum_md5(input.address, input.size)
                .map_err(|e| format!("Failed to compute checksum: {e}"))?;

            Ok(format!(
                "MD5 checksum of 0x{:x} bytes at 0x{:08x}: {:032x}",
                input.size, input.address, checksum
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }
}

/// Lowercase the first character of a summary so it reads after "Successfully ".
fn lower_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
