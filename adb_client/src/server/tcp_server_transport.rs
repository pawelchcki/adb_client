use std::io::{Error, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::str::FromStr;

use crate::ADBTransport;
use crate::models::{ADBCommand, AdbRequestStatus};
use crate::{Result, RustADBError};

const DEFAULT_SERVER_IP: Ipv4Addr = Ipv4Addr::LOCALHOST;
const DEFAULT_SERVER_PORT: u16 = 5037;

/// Server transport running on top on TCP
#[derive(Debug)]
pub struct TCPServerTransport {
    socket_addr: SocketAddrV4,
    tcp_stream: Option<TcpStream>,
    timeouts: crate::ADBTimeouts,
}

impl Default for TCPServerTransport {
    fn default() -> Self {
        Self::new(SocketAddrV4::new(DEFAULT_SERVER_IP, DEFAULT_SERVER_PORT))
    }
}

impl TCPServerTransport {
    /// Instantiates a new instance of [`TCPServerTransport`]
    #[must_use]
    pub const fn new(socket_addr: SocketAddrV4) -> Self {
        Self {
            socket_addr,
            tcp_stream: None,
            timeouts: crate::ADBTimeouts {
                write: None,
                ..crate::ADBTimeouts::DEFAULT
            },
        }
    }

    /// Instantiate a new instance of [`TCPServerTransport`] using given address, or default if not specified.
    #[must_use]
    pub fn new_or_default(socket_addr: Option<SocketAddrV4>) -> Self {
        socket_addr.map_or_else(Self::default, Self::new)
    }

    /// Get underlying [`SocketAddrV4`]
    #[must_use]
    pub const fn get_socketaddr(&self) -> SocketAddrV4 {
        self.socket_addr
    }

    /// Configure connection and command I/O timeouts, including an existing socket.
    pub fn set_timeouts(&mut self, timeouts: crate::ADBTimeouts) -> Result<()> {
        let timeouts = timeouts.validate()?;
        if let Some(stream) = &self.tcp_stream {
            stream.set_read_timeout(timeouts.read)?;
            stream.set_write_timeout(timeouts.write)?;
        }
        self.timeouts = timeouts;
        Ok(())
    }

    pub(crate) fn proxy_connection(
        &self,
        adb_command: &ADBCommand,
        with_response: bool,
    ) -> Result<Vec<u8>> {
        self.send_adb_request(adb_command)?;

        if with_response {
            let length = self.get_hex_body_length()?;
            let mut body = vec![
                0;
                length
                    .try_into()
                    .map_err(|_| RustADBError::ConversionError)?
            ];
            if length > 0 {
                self.get_raw_connection()?.read_exact(&mut body)?;
            }

            Ok(body)
        } else {
            Ok(vec![])
        }
    }

    pub(crate) fn get_raw_connection(&self) -> Result<&TcpStream> {
        self.tcp_stream
            .as_ref()
            .ok_or(RustADBError::IOError(Error::new(
                ErrorKind::NotConnected,
                "not connected",
            )))
    }

    /// Gets the body length from hexadecimal value
    pub(crate) fn get_hex_body_length(&self) -> Result<u32> {
        let length_buffer = self.read_body_length()?;
        Ok(u32::from_str_radix(
            std::str::from_utf8(&length_buffer)?,
            16,
        )?)
    }

    /// Read 4 bytes representing body length
    fn read_body_length(&self) -> Result<[u8; 4]> {
        let mut length_buffer = [0; 4];
        self.get_raw_connection()?.read_exact(&mut length_buffer)?;

        Ok(length_buffer)
    }

    /// Send the given [`AdbCommand`] to ADB server, and checks that the request has been taken in consideration.
    /// If an error occurred, a [`RustADBError`] is returned with the response error string.
    pub(crate) fn send_adb_request(&self, command: &ADBCommand) -> Result<()> {
        self.send_service_request(&command.to_string())
    }

    pub(crate) fn take_connection(&mut self) -> Result<TcpStream> {
        self.tcp_stream
            .take()
            .ok_or_else(|| Error::from(ErrorKind::NotConnected).into())
    }

    pub(crate) fn send_service_request(&self, service: &str) -> Result<()> {
        if service.len() > 0xffff || service.contains('\0') {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "ADB service must be at most 65535 bytes and contain no NUL",
            )
            .into());
        }
        let request = format!("{:04x}{service}", service.len());
        self.get_raw_connection()?.write_all(request.as_bytes())?;

        self.read_adb_response()
    }

    /// Read a response from ADB server
    pub(crate) fn read_adb_response(&self) -> Result<()> {
        // Reads returned status code from ADB server
        let mut request_status = [0; 4];
        self.get_raw_connection()?.read_exact(&mut request_status)?;

        match AdbRequestStatus::from_str(std::str::from_utf8(request_status.as_ref())?)? {
            AdbRequestStatus::Fail => {
                // We can keep reading to get further details
                let length = self.get_hex_body_length()?;

                let mut body = vec![
                    0;
                    length
                        .try_into()
                        .map_err(|_| RustADBError::ConversionError)?
                ];
                if length > 0 {
                    self.get_raw_connection()?.read_exact(&mut body)?;
                }

                Err(RustADBError::ADBRequestFailed(String::from_utf8(body)?))
            }
            AdbRequestStatus::Okay => Ok(()),
        }
    }
}

impl ADBTransport for TCPServerTransport {
    fn disconnect(&mut self) -> Result<()> {
        if let Some(conn) = &mut self.tcp_stream {
            conn.shutdown(std::net::Shutdown::Both)?;
            log::trace!("Disconnected from {}", conn.peer_addr()?);
        }

        Ok(())
    }

    fn connect(&mut self) -> Result<()> {
        if let Some(previous) = &self.tcp_stream {
            // Ignoring underlying error, we will recreate a new connection
            let _ = previous.shutdown(std::net::Shutdown::Both);
        }
        let tcp_stream = match self.timeouts.handshake {
            Some(timeout) => TcpStream::connect_timeout(&self.socket_addr.into(), timeout)?,
            None => TcpStream::connect(self.socket_addr)?,
        };
        tcp_stream.set_read_timeout(self.timeouts.read)?;
        tcp_stream.set_write_timeout(self.timeouts.write)?;
        tcp_stream.set_nodelay(true)?;
        self.tcp_stream = Some(tcp_stream);
        log::trace!("Successfully connected to {}", self.socket_addr);

        Ok(())
    }
}
