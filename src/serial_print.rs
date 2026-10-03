//! Printing to a serial / Bluetooth-SPP receipt printer.
//!
//! A Bluetooth mini thermal printer, once paired in Windows (Settings -> Bluetooth & devices),
//! normally appears as an outgoing "Standard Serial over Bluetooth link (COMx)" port. Writing
//! raw ESC/POS bytes to that port prints, with no Windows printer driver involved. Opening the
//! port is what makes Windows connect to the printer, so it must be switched on and in range.

use serde::Serialize;
use serialport::SerialPortType;
use std::io::Write;
use std::time::Duration;

const OPEN_TIMEOUT: Duration = Duration::from_secs(12);
// Mini printers have a very small receive buffer: small chunks + short pauses keep them from
// dropping the middle of a receipt.
const CHUNK: usize = 256;
const CHUNK_PAUSE: Duration = Duration::from_millis(25);
// Let the printer finish what it has been sent before the port is closed.
const DRAIN_PAUSE: Duration = Duration::from_millis(400);

#[derive(Serialize)]
pub struct SerialPortEntry {
    name: String,
    description: String,
    bluetooth: bool,
}

#[derive(Serialize)]
pub struct SerialPortStatus {
    present: bool,
    detail: String,
}

fn com_number(name: &str) -> u32 {
    name.trim_start_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(u32::MAX)
}

#[tauri::command]
pub fn list_serial_ports() -> Result<Vec<SerialPortEntry>, String> {
    let ports = serialport::available_ports().map_err(|e| format!("Could not list COM ports — {e}"))?;
    let mut out: Vec<SerialPortEntry> = ports
        .into_iter()
        .map(|p| {
            let (description, bluetooth) = match &p.port_type {
                SerialPortType::BluetoothPort => ("Bluetooth".to_string(), true),
                SerialPortType::UsbPort(u) => (u.product.clone().unwrap_or_else(|| "USB serial".to_string()), false),
                SerialPortType::PciPort => ("PCI serial".to_string(), false),
                SerialPortType::Unknown => ("Serial port".to_string(), false),
            };
            SerialPortEntry { name: p.port_name, description, bluetooth }
        })
        .collect();
    // Bluetooth ports first, then by COM number (COM3 before COM10).
    out.sort_by(|a, b| b.bluetooth.cmp(&a.bluetooth).then(com_number(&a.name).cmp(&com_number(&b.name))));
    Ok(out)
}

/// Does NOT open the port: opening a Bluetooth COM port connects to the printer, which is slow
/// and drains its battery if done on a timer. It only confirms the port is still paired/present.
#[tauri::command]
pub fn check_serial_port(port: String) -> Result<SerialPortStatus, String> {
    let ports = serialport::available_ports().map_err(|e| format!("Could not list COM ports — {e}"))?;
    let found = ports.iter().any(|p| p.port_name.eq_ignore_ascii_case(&port));
    Ok(SerialPortStatus {
        present: found,
        detail: if found {
            format!("{port} is available (the printer must be switched on and in range to print)")
        } else {
            format!("{port} was not found. Pair the printer again in Windows Bluetooth settings, then pick its port.")
        },
    })
}

fn write_serial(port: &str, baud: u32, data: &[u8]) -> Result<(), String> {
    if data.is_empty() {
        return Ok(());
    }
    let mut sp = serialport::new(port, baud)
        .timeout(OPEN_TIMEOUT)
        .open()
        .map_err(|e| format!("Could not open {port} — {e}. Check that the printer is switched on, in range and paired."))?;

    for chunk in data.chunks(CHUNK) {
        sp.write_all(chunk)
            .map_err(|e| format!("Connected to {port} but the print job failed partway — {e}"))?;
        sp.flush().map_err(|e| format!("Could not send the print job to {port} — {e}"))?;
        std::thread::sleep(CHUNK_PAUSE);
    }
    std::thread::sleep(DRAIN_PAUSE);
    Ok(())
}

#[tauri::command]
pub async fn print_raw_serial(port: String, baud: u32, data: Vec<u8>) -> Result<(), String> {
    let baud = if (1200..=921_600).contains(&baud) { baud } else { 9600 };
    tauri::async_runtime::spawn_blocking(move || write_serial(&port, baud, &data))
        .await
        .map_err(|e| e.to_string())?
}
