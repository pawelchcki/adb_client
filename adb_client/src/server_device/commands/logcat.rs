use crate::{Result, server_device::ADBServerDevice};
use std::io::{self, Write};
use std::net::TcpStream;

impl ADBServerDevice {
    /// Open a device service and transfer ownership of its socket to the caller.
    ///
    /// The returned stream remains usable after this device is dropped or used
    /// for another command. Dropping the stream cancels the service.
    pub fn open_service(&mut self, service: &str) -> Result<TcpStream> {
        if service.len() > 0xffff || service.contains('\0') {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "invalid ADB service name").into(),
            );
        }
        self.set_serial_transport()?;
        self.transport.send_service_request(service)?;
        self.transport.take_connection()
    }

    /// Stream logcat output as a reader. Use `BufRead` for lines, or drop to cancel.
    pub fn log_reader(&mut self) -> Result<TcpStream> {
        self.open_service("exec:logcat")
    }

    /// Copy logcat bytes to a writer, preserving partial lines and I/O errors.
    pub fn get_logs<W: Write>(&mut self, mut output: W) -> Result<()> {
        io::copy(&mut self.log_reader()?, &mut output)?;
        output.flush()?;
        Ok(())
    }
}
