//! Nonblocking ADB-server access. Each service owns a Tokio TCP socket, so a
//! long-running logcat does not occupy a blocking thread or serialize commands.
//! Drop a returned stream or its future to cancel. Native USB/TCP device APIs
//! remain synchronous; browser WebUSB has its own asynchronous API.

use crate::{ADBTimeouts, Result, RustADBError, server::DeviceShort};
use std::{future::Future, io, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn io_timeout<T>(
    limit: Option<Duration>,
    operation: impl Future<Output = io::Result<T>>,
) -> Result<T> {
    match limit {
        Some(limit) => tokio::time::timeout(limit, operation)
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
            .map_err(Into::into),
        None => operation.await.map_err(Into::into),
    }
}

fn validate(service: &str) -> Result<()> {
    if service.len() > 0xffff || service.contains('\0') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid ADB service name").into());
    }
    Ok(())
}

async fn body(stream: &mut TcpStream, timeout: Option<Duration>) -> Result<Vec<u8>> {
    let mut length = [0; 4];
    io_timeout(timeout, stream.read_exact(&mut length)).await?;
    let length = usize::from_str_radix(std::str::from_utf8(&length)?, 16)?;
    let mut bytes = vec![0; length];
    io_timeout(timeout, stream.read_exact(&mut bytes)).await?;
    Ok(bytes)
}

async fn request(stream: &mut TcpStream, service: &str, timeouts: ADBTimeouts) -> Result<()> {
    validate(service)?;
    let request = format!("{:04x}{service}", service.len());
    io_timeout(timeouts.write, stream.write_all(request.as_bytes())).await?;
    let mut status = [0; 4];
    io_timeout(timeouts.read, stream.read_exact(&mut status)).await?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => Err(RustADBError::ADBRequestFailed(String::from_utf8(
            body(stream, timeouts.read).await?,
        )?)),
        _ => Err(RustADBError::UnknownResponseType(
            String::from_utf8_lossy(&status).into_owned(),
        )),
    }
}

/// Asynchronous connection factory for a running ADB server.
#[derive(Clone, Debug)]
pub struct AsyncADBServer {
    address: SocketAddr,
    timeouts: ADBTimeouts,
}

impl Default for AsyncADBServer {
    fn default() -> Self {
        Self::new(SocketAddr::from(([127, 0, 0, 1], 5037)))
    }
}

impl AsyncADBServer {
    /// Select an ADB server address without performing I/O.
    #[must_use]
    pub const fn new(address: SocketAddr) -> Self {
        Self {
            address,
            timeouts: ADBTimeouts {
                write: None,
                ..ADBTimeouts::DEFAULT
            },
        }
    }

    /// Configure connection and helper-operation timeouts.
    pub fn with_timeouts(mut self, timeouts: ADBTimeouts) -> Result<Self> {
        self.timeouts = timeouts.validate()?;
        Ok(self)
    }

    async fn connect(&self) -> Result<TcpStream> {
        let stream = io_timeout(self.timeouts.handshake, TcpStream::connect(self.address)).await?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    /// List devices without blocking the executor.
    pub async fn devices(&self) -> Result<Vec<DeviceShort>> {
        let mut stream = self.connect().await?;
        request(&mut stream, "host:devices", self.timeouts).await?;
        body(&mut stream, self.timeouts.read)
            .await?
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| DeviceShort::try_from(line.to_vec()))
            .collect()
    }

    /// Select a device by serial. Concurrent operations use independent sockets.
    #[must_use]
    pub fn device(&self, identifier: impl Into<String>) -> AsyncADBServerDevice {
        AsyncADBServerDevice {
            server: self.clone(),
            selection: format!("host:transport:{}", identifier.into()),
        }
    }

    /// Select a device by its current, volatile server transport ID.
    #[must_use]
    pub fn device_by_transport_id(&self, id: u32) -> AsyncADBServerDevice {
        AsyncADBServerDevice {
            server: self.clone(),
            selection: format!("host:transport-id:{id}"),
        }
    }
}

/// An asynchronous ADB-server device selected by serial or transport ID.
#[derive(Clone, Debug)]
pub struct AsyncADBServerDevice {
    server: AsyncADBServer,
    selection: String,
}

impl AsyncADBServerDevice {
    /// Open a service as an owned, cancellable `AsyncRead + AsyncWrite` socket.
    ///
    /// Dropping this device does not close a returned stream. Timeout settings
    /// cover connection/service setup; callers control subsequent stream deadlines.
    pub async fn open_service(&self, service: &str) -> Result<TcpStream> {
        validate(service)?;
        validate(&self.selection)?;
        let mut stream = self.server.connect().await?;
        request(&mut stream, &self.selection, self.server.timeouts).await?;
        request(&mut stream, service, self.server.timeouts).await?;
        Ok(stream)
    }

    /// Read unfiltered logcat bytes; use `tokio::io::BufReader` for lines.
    pub async fn log_reader(&self) -> Result<TcpStream> {
        self.open_service("exec:logcat").await
    }

    /// Open a raw command-output stream without a PTY or stdin-copy task.
    pub async fn exec_stream(&self, command: &str) -> Result<TcpStream> {
        self.open_service(&format!("exec:{command}")).await
    }

    /// Collect raw command output, preserving every binary byte.
    pub async fn exec_out(&self, command: &str) -> Result<Vec<u8>> {
        self.collect(self.exec_stream(command).await?).await
    }

    /// Collect legacy shell stdout/stderr. This service does not report an exit status.
    pub async fn shell_command(&self, command: &str) -> Result<Vec<u8>> {
        self.collect(self.open_service(&format!("shell:{command}")).await?)
            .await
    }

    async fn collect(&self, mut stream: TcpStream) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let size = io_timeout(self.server.timeouts.read, stream.read(&mut buffer)).await?;
            if size == 0 {
                return Ok(output);
            }
            output.extend_from_slice(&buffer[..size]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream as StdStream},
    };

    fn request(stream: &mut StdStream, expected: &str) {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let length = usize::from_str_radix(std::str::from_utf8(&length).unwrap(), 16).unwrap();
        let mut request = vec![0; length];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(request, expected.as_bytes());
        stream.write_all(b"OKAY").unwrap();
    }

    #[test]
    fn pending_log_read_does_not_block_a_single_thread_executor() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut logs, _) = listener.accept().unwrap();
            request(&mut logs, "host:transport:selected");
            request(&mut logs, "exec:logcat");
            let (mut exec, _) = listener.accept().unwrap();
            request(&mut exec, "host:transport:selected");
            request(&mut exec, "exec:screencap -p");
            exec.write_all(b"\x89PNG\r\n\0\xff").unwrap();
            logs.write_all(b"last line without newline").unwrap();
        });
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let device = AsyncADBServer::new(address).device("selected");
                let mut logs = device.log_reader().await.unwrap();
                let read = tokio::spawn(async move {
                    let mut output = Vec::new();
                    logs.read_to_end(&mut output).await.unwrap();
                    output
                });
                assert_eq!(
                    device.exec_out("screencap -p").await.unwrap(),
                    b"\x89PNG\r\n\0\xff"
                );
                assert_eq!(read.await.unwrap(), b"last line without newline");
            });
        peer.join().unwrap();
    }
}
