use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    sync::Mutex,
    time::{Duration, Instant},
};

use rustls::ClientConnection;

use crate::{
    Result, RustADBError,
    message_devices::{
        adb_message_transport::DEFAULT_READ_TIMEOUT, adb_transport_message::ADBTransportMessage,
        packet_reader::PacketReader,
    },
};

#[derive(Debug, Default)]
struct ReceiveBuffer {
    encrypted: Vec<u8>,
    consumed: usize,
}

#[derive(Debug, Default)]
struct ReceiveState {
    tls_input: ReceiveBuffer,
    packet: PacketReader,
}

/// Message readers and writers serialize independently. TLS state is locked only
/// while processing memory buffers, never during blocking socket I/O after upgrade.
#[derive(Debug)]
pub(super) struct Connection {
    socket: TcpStream,
    tls: Mutex<Option<Box<ClientConnection>>>,
    receive: Mutex<ReceiveState>,
    send: Mutex<Vec<u8>>,
}

impl Connection {
    pub(super) fn new(socket: TcpStream) -> Self {
        Self {
            socket,
            tls: Mutex::new(None),
            receive: Mutex::default(),
            send: Mutex::default(),
        }
    }

    pub(super) fn is_tls(&self) -> Result<bool> {
        Ok(self.tls.lock()?.is_some())
    }

    pub(super) fn upgrade(
        &self,
        mut client: ClientConnection,
        deadline: Option<Instant>,
    ) -> Result<()> {
        // An upgrade owns both directions until the handshake is complete.
        let _receive = self.receive.lock()?;
        let _send = self.send.lock()?;
        let mut tls = self.tls.lock()?;
        if tls.is_some() {
            return Err(RustADBError::UpgradeError(
                "cannot upgrade a TLS connection".into(),
            ));
        }
        client.complete_io(&mut HandshakeIo {
            socket: &self.socket,
            deadline,
        })?;
        *tls = Some(Box::new(client));
        Ok(())
    }

    pub(super) fn disconnect(&self) -> Result<()> {
        if let Some(tls) = self.tls.lock()?.as_mut() {
            tls.send_close_notify();
        }
        let _ = self.socket.shutdown(Shutdown::Both);
        Ok(())
    }

    pub(super) fn read_message(&self, timeout: Duration) -> Result<ADBTransportMessage> {
        let mut state = self.receive.lock()?;
        self.socket
            .set_read_timeout((timeout != DEFAULT_READ_TIMEOUT).then_some(timeout))?;
        let ReceiveState { tls_input, packet } = &mut *state;
        let mut reader = ConnectionReader {
            connection: self,
            pending: tls_input,
        };
        packet.read(|bytes| reader.read(bytes).map_err(Into::into))
    }

    pub(super) fn write_message(
        &self,
        message: &ADBTransportMessage,
        timeout: Duration,
    ) -> Result<()> {
        // Keep an entire ADB header/payload together across concurrent writers.
        let mut encrypted = self.send.lock()?;
        self.socket
            .set_write_timeout((timeout != DEFAULT_READ_TIMEOUT).then_some(timeout))?;
        let mut writer = ConnectionWriter {
            connection: self,
            encrypted: &mut encrypted,
        };
        writer.write_all(&message.header().as_bytes())?;
        writer.write_all(message.payload())?;
        writer.flush()?;
        Ok(())
    }
}

struct ConnectionReader<'a> {
    connection: &'a Connection,
    pending: &'a mut ReceiveBuffer,
}

impl Read for ConnectionReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let mut state = self
                .connection
                .tls
                .lock()
                .map_err(|_| io::Error::other("TLS state mutex was poisoned"))?;
            let Some(tls) = state.as_mut() else {
                drop(state);
                return (&self.connection.socket).read(output);
            };
            match tls.reader().read(output) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                result => return result,
            }
            if self.pending.consumed < self.pending.encrypted.len() {
                let used = tls.read_tls(&mut &self.pending.encrypted[self.pending.consumed..])?;
                if used == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "TLS input made no progress",
                    ));
                }
                self.pending.consumed += used;
                tls.process_new_packets()
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                continue;
            }
            // TLS is already handshaken. Any post-handshake response is queued
            // for the next writer; receiving never waits for the send lock.
            drop(state);
            self.pending.encrypted.resize(16 * 1024, 0);
            let size = (&self.connection.socket).read(&mut self.pending.encrypted)?;
            if size == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            self.pending.encrypted.truncate(size);
            self.pending.consumed = 0;
        }
    }
}

struct ConnectionWriter<'a> {
    connection: &'a Connection,
    encrypted: &'a mut Vec<u8>,
}

fn drain_tls(tls: &mut ClientConnection, encrypted: &mut Vec<u8>) -> io::Result<()> {
    encrypted.clear();
    while tls.wants_write() {
        tls.write_tls(encrypted)?;
    }
    Ok(())
}

impl Write for ConnectionWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self
            .connection
            .tls
            .lock()
            .map_err(|_| io::Error::other("TLS state mutex was poisoned"))?;
        let Some(tls) = state.as_mut() else {
            drop(state);
            return (&self.connection.socket).write(bytes);
        };
        let size = tls.writer().write(bytes)?;
        drain_tls(tls, self.encrypted)?;
        drop(state);
        (&self.connection.socket).write_all(self.encrypted)?;
        Ok(size)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self
            .connection
            .tls
            .lock()
            .map_err(|_| io::Error::other("TLS state mutex was poisoned"))?;
        if let Some(tls) = state.as_mut() {
            drain_tls(tls, self.encrypted)?;
            drop(state);
            (&self.connection.socket).write_all(self.encrypted)?;
        } else {
            drop(state);
        }
        (&self.connection.socket).flush()
    }
}

// rustls can perform many reads/writes inside complete_io. Recompute the
// remaining budget at every syscall so partial TLS records cannot extend it.
struct HandshakeIo<'a> {
    socket: &'a TcpStream,
    deadline: Option<Instant>,
}
impl HandshakeIo<'_> {
    fn remaining(&self) -> io::Result<Option<Duration>> {
        self.deadline
            .map(|deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|duration| !duration.is_zero())
                    .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
            })
            .transpose()
    }
}
impl Read for HandshakeIo<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(self.remaining()?)?;
        self.socket.read(bytes)
    }
}
impl Write for HandshakeIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(self.remaining()?)?;
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.remaining()?;
        self.socket.flush()
    }
}
