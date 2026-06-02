//! espflash backend: serial connection, IDF-format flashing, and the
//! [`ByteSource`] over a raw serial port.

use crate::capture::ByteSource;
use espflash::{
    connection::{Connection, ResetAfterOperation, ResetBeforeOperation},
    flasher::{FlashData, FlashSettings, Flasher},
    image_format::{ImageFormat, idf::IdfBootloaderFormat},
    target::DefaultProgressCallback,
};
use serialport::{ClearBuffer, SerialPortType};
use std::path::Path;
use std::time::Duration;

/// Resolve the serial port: explicit wins; otherwise auto-detect when exactly
/// one USB serial port is present, else error listing the candidates.
pub fn detect_serial_port(explicit: Option<&str>) -> Result<String, String> {
    if let Some(p) = explicit {
        return Ok(p.to_string());
    }
    let ports = serialport::available_ports()
        .map_err(|e| format!("Failed to enumerate serial ports: {e}"))?;
    let usb: Vec<String> = ports
        .iter()
        .filter(|p| matches!(p.port_type, SerialPortType::UsbPort(_)))
        .map(|p| p.port_name.clone())
        .collect();
    match usb.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err("no USB serial ports found; connect a device or pass `port`".to_string()),
        many => Err(format!(
            "multiple serial ports ({}); pass `port` to choose one",
            many.join(", ")
        )),
    }
}

/// Connect to an ESP device over serial and return a connected [`Flasher`].
pub fn connect_to_device(port_name: &str, baud: u32, use_stub: bool) -> Result<Flasher, String> {
    let ports = serialport::available_ports()
        .map_err(|e| format!("Failed to enumerate serial ports: {e}"))?;

    let port_info = ports.iter().find(|p| p.port_name == port_name);

    let usb_info = match port_info.map(|p| &p.port_type) {
        Some(SerialPortType::UsbPort(info)) => info.clone(),
        _ => serialport::UsbPortInfo {
            vid: 0,
            pid: 0,
            serial_number: None,
            manufacturer: None,
            product: None,
            interface: None,
        },
    };

    let serial = serialport::new(port_name, 115_200)
        .open_native()
        .map_err(|e| format!("Failed to open serial port '{port_name}': {e}"))?;

    let connection = Connection::new(
        serial,
        usb_info,
        ResetAfterOperation::HardReset,
        ResetBeforeOperation::DefaultReset,
        115_200,
    );

    Flasher::connect(
        connection,
        use_stub,
        true, // verify
        true, // skip unchanged
        None, // auto-detect chip
        if baud > 115_200 { Some(baud) } else { None },
    )
    .map_err(|e| format!("Failed to connect to device: {e}"))
}

/// Flash `file_data` to the connected device. With `flash_address`, the data is
/// written verbatim as a raw binary; otherwise it is treated as an ELF and run
/// through the IDF bootloader format. Returns a human-readable summary line.
pub fn flash_file(
    flasher: &mut Flasher,
    file_data: &[u8],
    flash_address: Option<u32>,
    partition_table: Option<&str>,
    bootloader: Option<&str>,
) -> Result<String, String> {
    let info = flasher
        .device_info()
        .map_err(|e| format!("Failed to get device info: {e}"))?;

    let summary = if let Some(addr) = flash_address {
        flasher
            .write_bin_to_flash(addr, file_data, &mut DefaultProgressCallback)
            .map_err(|e| format!("Failed to write binary to flash: {e}"))?;
        format!("Flashed {} bytes to 0x{:08x}", file_data.len(), addr)
    } else {
        let flash_data = FlashData::new(
            FlashSettings::default(),
            0,    // min_chip_rev
            None, // mmu_page_size (auto)
            info.chip,
            info.crystal_frequency,
        );

        let image = IdfBootloaderFormat::new(
            file_data,
            &flash_data,
            partition_table.map(Path::new),
            bootloader.map(Path::new),
            None, // partition_table_offset
            None, // target_app_partition
        )
        .map_err(|e| format!("Failed to create flash image: {e}"))?;

        flasher
            .load_image_to_flash(&mut DefaultProgressCallback, ImageFormat::EspIdf(image))
            .map_err(|e| format!("Failed to flash image: {e}"))?;

        format!("Flashed ELF ({} bytes) to {}", file_data.len(), info.chip)
    };

    // Reset into the freshly flashed app. espflash has no Drop impl, so dropping
    // the flasher does NOT reset — the chip would otherwise sit in the download
    // mode it was flashed in and never run the app. Mirror the espflash CLI,
    // which calls `reset_after(is_stub, chip)` (we always connect with the stub).
    flasher
        .connection()
        .reset_after(true, info.chip)
        .map_err(|e| format!("Failed to reset into app after flash: {e}"))?;

    Ok(summary)
}

/// A [`ByteSource`] over a raw serial port, used by the capture pipeline. The
/// 100 ms read timeout paces the loop, so timed-out reads map to `Ok(0)` and the
/// idle nap stays zero.
pub struct SerialSource {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialSource {
    pub fn open(port_name: &str, baud: u32) -> Result<Self, String> {
        let port = serialport::new(port_name, baud)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| format!("Failed to open serial port '{port_name}': {e}"))?;
        Ok(Self { port })
    }
}

impl ByteSource for SerialSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Read as _;
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn flush_input(&mut self) -> std::io::Result<()> {
        self.port
            .clear(ClearBuffer::Input)
            .map_err(std::io::Error::other)
    }
}
