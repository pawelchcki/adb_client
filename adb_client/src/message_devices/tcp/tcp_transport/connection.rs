use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    sync::Mutex,
    time::Duration,
};

use rustls::ClientConnection;

use crate::{
    Result, RustADBError,
    message_devices::adb_transport_message::{ADBTransportMessage, ADBTransportMessageHeader},
};

#[derive(Debug, Default)]
struct ReceiveBuffer {
    encrypted: Vec<u8>,
    consumed: usize,
}

/// Message readers and writers serialize independently. TLS state is locked only
/// while processing memory buffers, never during blocking socket I/O after upgrade.
#[derive(Debug)]
pub(super) struct Connection {
    socket: TcpStream,
    tls: Mutex<Option<Box<ClientConnection>>>,
    receive: Mutex<ReceiveBuffer>,
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

    pub(super) fn upgrade(&self, mut client: ClientConnection) -> Result<()> {
        // An upgrade owns both directions until the handshake is complete.
        let _receive = self.receive.lock()?;
        let _send = self.send.lock()?;
        let mut tls = self.tls.lock()?;
        if tls.is_some() {
            return Err(RustADBError::UpgradeError(
                "cannot upgrade a TLS connection".into(),
            ));
        }
        client.complete_io(&mut &self.socket)?;
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
        let mut pending = self.receive.lock()?;
        self.socket.set_read_timeout(Some(timeout))?;
        let mut reader = ConnectionReader {
            connection: self,
            pending: &mut pending,
        };
        let mut bytes = [0; 24];
        reader.read_exact(&mut bytes)?;
        let header = ADBTransportMessageHeader::try_from(bytes)?;
        let mut payload = vec![0; header.data_length() as usize];
        reader.read_exact(&mut payload)?;
        let message = ADBTransportMessage::from_header_and_payload(header, payload);
        if !message.payload().is_empty() && !message.check_message_integrity() {
            return Err(RustADBError::InvalidIntegrity(
                ADBTransportMessageHeader::compute_crc32(message.payload()),
                message.header().data_crc32(),
            ));
        }
        Ok(message)
    }

    pub(super) fn write_message(
        &self,
        message: &ADBTransportMessage,
        timeout: Duration,
    ) -> Result<()> {
        // Keep an entire ADB header/payload together across concurrent writers.
        let mut encrypted = self.send.lock()?;
        self.socket.set_write_timeout(Some(timeout))?;
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
