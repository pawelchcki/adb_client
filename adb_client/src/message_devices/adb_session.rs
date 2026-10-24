use std::{
    io::{Cursor, Read, Seek},
    time::{Duration, SystemTime},
};

use byteorder::ReadBytesExt;

use crate::{
    AdbStatResponse, BinaryDecodable, Result, RustADBError,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_transport_message::{ADBTransportMessage, ADBTransportMessageHeader},
        message_commands::{MessageCommand, MessageSubcommand},
        utils::BinaryEncodable,
    },
};

/// Maximum size of the data of a sync `DATA` chunk.
const SYNC_DATA_MAX_SIZE: usize = 64 * 1024;

/// Size of a sync chunk header (identifier and length).
const SYNC_HEADER_SIZE: usize = 8;

/// Represent a session between an `ADBDevice` and remote `adbd`.
#[derive(Debug)]
pub struct ADBSession<T: ADBMessageTransport> {
    transport: T,
    local_id: u32,
    remote_id: u32,
    max_payload_size: usize,
}

impl<T: ADBMessageTransport> ADBSession<T> {
    /// Create a new session with the given transport and IDs.
    pub const fn new(transport: T, local_id: u32, remote_id: u32, max_payload_size: usize) -> Self {
        Self {
            transport,
            local_id,
            remote_id,
            max_payload_size,
        }
    }

    pub const fn get_transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub const fn local_id(&self) -> u32 {
        self.local_id
    }

    pub const fn remote_id(&self) -> u32 {
        self.remote_id
    }

    /// Maximum payload size of a message sent to the device.
    pub const fn max_payload_size(&self) -> usize {
        self.max_payload_size
    }

    /// Receive a message and acknowledge it by replying with an `OKAY` command
    pub(crate) fn recv_and_reply_okay(&mut self) -> Result<ADBTransportMessage> {
        let message = self.transport.read_message()?;
        self.transport.write_message(ADBTransportMessage::try_new(
            MessageCommand::Okay,
            self.local_id,
            self.remote_id,
            &[],
        )?)?;
        Ok(message)
    }

    /// Expect a message with an `OKAY` command after sending a message.
    pub(crate) fn send_and_expect_okay(
        &mut self,
        message: ADBTransportMessage,
    ) -> Result<ADBTransportMessage> {
        self.transport.write_message(message)?;

        self.transport.read_message().and_then(|message| {
            message.assert_command(MessageCommand::Okay)?;
            Ok(message)
        })
    }

    /// Receive the data sent by the device until it closes the session, and return it.
    ///
    /// `OKAY` messages only tell that the device is ready for more data, they are skipped. `adbd` sends one each
    /// time it forwards part of its queue to the service, so a single message can be acknowledged several times
    /// when the service reads slowly.
    pub(crate) fn recv_until_close(&mut self) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        loop {
            let message = self.transport.read_message()?;
            match message.header().command() {
                MessageCommand::Okay => {}
                MessageCommand::Write => {
                    output.extend_from_slice(message.payload());
                    self.transport.write_message(ADBTransportMessage::try_new(
                        MessageCommand::Okay,
                        self.local_id,
                        self.remote_id,
                        &[],
                    )?)?;
                }
                MessageCommand::Clse => return Ok(output),
                command => {
                    return Err(RustADBError::ADBRequestFailed(format!(
                        "Wrong command received {command}"
                    )));
                }
            }
        }
    }

    pub(crate) fn recv_file<W: std::io::Write>(
        &mut self,
        mut output: W,
    ) -> std::result::Result<(), RustADBError> {
        let mut len: Option<u64> = None;
        loop {
            let payload = self.recv_and_reply_okay()?.into_payload();
            let mut rdr = Cursor::new(&payload);
            while rdr.position() != payload.len() as u64 {
                match len.take() {
                    Some(0) | None => {
                        rdr.seek_relative(4)?;
                        len.replace(u64::from(rdr.read_u32::<byteorder::LittleEndian>()?));
                    }
                    Some(length) => {
                        let remaining_bytes = payload.len() as u64 - rdr.position();
                        if length < remaining_bytes {
                            std::io::copy(&mut rdr.by_ref().take(length), &mut output)?;
                        } else {
                            std::io::copy(&mut rdr.take(remaining_bytes), &mut output)?;
                            len.replace(length - remaining_bytes);
                            // this payload is now exhausted
                            break;
                        }
                    }
                }
            }
            if Cursor::new(&payload[(payload.len() - 8)..(payload.len() - 4)])
                .read_u32::<byteorder::LittleEndian>()?
                == MessageSubcommand::Done as u32
            {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn push_file<R: std::io::Read>(&mut self, mut reader: R) -> Result<()> {
        if self.max_payload_size <= SYNC_HEADER_SIZE {
            return Err(RustADBError::ADBRequestFailed(
                "Negotiated payload is too small for sync DATA".into(),
            ));
        }
        // Read directly into one coalesced sync payload and reuse its allocation
        // after each acknowledged write. Preserve the negotiated message limit.
        let mut payload = vec![0; self.max_payload_size];
        let mut eof = false;
        while !eof {
            payload.resize(self.max_payload_size, 0);
            let mut used = 0;
            while used + SYNC_HEADER_SIZE < self.max_payload_size {
                let capacity =
                    (self.max_payload_size - used - SYNC_HEADER_SIZE).min(SYNC_DATA_MAX_SIZE);
                let size = reader.read(
                    &mut payload[used + SYNC_HEADER_SIZE..used + SYNC_HEADER_SIZE + capacity],
                )?;
                if size == 0 {
                    eof = true;
                    break;
                }
                payload[used..used + 4].copy_from_slice(b"DATA");
                payload[used + 4..used + 8].copy_from_slice(&u32::try_from(size)?.to_le_bytes());
                used += SYNC_HEADER_SIZE + size;
            }
            if used != 0 {
                payload.truncate(used);
                let header = ADBTransportMessageHeader::try_new(
                    MessageCommand::Write,
                    self.local_id,
                    self.remote_id,
                    &payload,
                )?;
                let message = ADBTransportMessage::from_header_and_payload(header, payload);
                self.transport.write_message_ref(&message)?;
                self.transport
                    .read_message()?
                    .assert_command(MessageCommand::Okay)?;
                payload = message.into_payload();
            }
        }

        // `DONE` carries the file modification time: a stream has none, the file is dated from now
        let Ok(last_modified) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) else {
            return Err(RustADBError::ADBRequestFailed(
                "SystemTime before unix epoch".into(),
            ));
        };
        let subcommand_data =
            MessageSubcommand::Done.with_arg(u32::try_from(last_modified.as_secs())?);

        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id(),
            self.remote_id(),
            &subcommand_data.encode(),
        )?;

        self.send_and_expect_okay(message)?;

        // Command should end with a Write => Okay
        let received = self.transport.read_message()?;
        match received.header().command() {
            MessageCommand::Write => Ok(()),
            c => Err(RustADBError::ADBRequestFailed(format!(
                "Wrong command received {c}"
            ))),
        }
    }

    pub(crate) fn stat_with_explicit_ids(&mut self, remote_path: &str) -> Result<AdbStatResponse> {
        let stat_buffer = MessageSubcommand::Stat.with_arg(u32::try_from(remote_path.len())?);
        let message = ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id(),
            self.remote_id(),
            &stat_buffer.encode(),
        )?;
        self.send_and_expect_okay(message)?;
        self.send_and_expect_okay(ADBTransportMessage::try_new(
            MessageCommand::Write,
            self.local_id(),
            self.remote_id(),
            remote_path.as_bytes(),
        )?)?;

        let response = self.transport.read_message()?;
        // Skip first 4 bytes as this is the literal "STAT".
        // Interesting part starts right after

        AdbStatResponse::decode(&response.into_payload()[4..])
    }
}

impl<T: ADBMessageTransport> Drop for ADBSession<T> {
    fn drop(&mut self) {
        // some devices will repeat the trailing CLSE command to ensure
        // the client has acknowledged it. Read them quickly if present.
        while let Ok(_discard_close_message) = self
            .transport
            .read_message_with_timeout(Duration::from_millis(20))
        {}
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    use crate::adb_transport::ADBTransport;
    use std::{
        collections::VecDeque,
        io,
        sync::{Arc, Mutex},
    };

    #[derive(Default)]
    struct State {
        replies: VecDeque<ADBTransportMessage>,
        frames: Vec<(Vec<u8>, usize)>,
    }
    #[derive(Clone)]
    struct Capture(Arc<Mutex<State>>);
    impl ADBTransport for Capture {
        fn connect(&mut self) -> Result<()> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }
    impl ADBMessageTransport for Capture {
        fn read_message_with_timeout(&mut self, _: Duration) -> Result<ADBTransportMessage> {
            self.0
                .lock()
                .unwrap()
                .replies
                .pop_front()
                .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof).into())
        }
        fn write_message_with_timeout(
            &mut self,
            message: ADBTransportMessage,
            timeout: Duration,
        ) -> Result<()> {
            self.write_message_ref_with_timeout(&message, timeout)
        }
        fn write_message_ref_with_timeout(
            &mut self,
            message: &ADBTransportMessage,
            _: Duration,
        ) -> Result<()> {
            assert!(message.check_message_integrity());
            assert_eq!(message.header().command(), MessageCommand::Write);
            let mut state = self.0.lock().unwrap();
            state.frames.push((
                message.payload().to_vec(),
                message.payload().as_ptr() as usize,
            ));
            state.replies.push_back(ADBTransportMessage::try_new(
                MessageCommand::Okay,
                2,
                1,
                &[],
            )?);
            if message.payload().starts_with(b"DONE") {
                state.replies.push_back(ADBTransportMessage::try_new(
                    MessageCommand::Write,
                    2,
                    1,
                    b"OKAY\0\0\0\0",
                )?);
            }
            Ok(())
        }
    }
    struct ShortRead {
        bytes: Cursor<Vec<u8>>,
        chunk: usize,
    }
    impl Read for ShortRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = buffer.len().min(self.chunk);
            self.bytes.read(&mut buffer[..count])
        }
    }
    #[test]
    fn upload_reuses_coalesced_frames_with_empty_files_short_reads_and_negotiated_limits() {
        for maximum in [4096, 65544, 1048576] {
            for size in [0, maximum * 2 + 123] {
                for chunk in [17, usize::MAX] {
                    let bytes: Vec<_> = (0..size).map(|n| u8::try_from(n % 251).unwrap()).collect();
                    let capture = Capture(Arc::default());
                    let mut session = ADBSession::new(capture.clone(), 1, 2, maximum);
                    session
                        .push_file(ShortRead {
                            bytes: Cursor::new(bytes.clone()),
                            chunk,
                        })
                        .unwrap();
                    let state = capture.0.lock().unwrap();
                    let mut received = Vec::new();
                    let frames = &state.frames[..state.frames.len() - 1];
                    for (payload, address) in frames {
                        assert!(!payload.is_empty() && payload.len() <= maximum);
                        assert_eq!(*address, frames[0].1);
                        let mut offset = 0;
                        while offset < payload.len() {
                            assert_eq!(&payload[offset..offset + 4], b"DATA");
                            let count = u32::from_le_bytes(
                                payload[offset + 4..offset + 8].try_into().unwrap(),
                            ) as usize;
                            assert!(count != 0 && count <= SYNC_DATA_MAX_SIZE);
                            received.extend_from_slice(&payload[offset + 8..offset + 8 + count]);
                            offset += 8 + count;
                        }
                    }
                    assert_eq!(received, bytes);
                    let done = &state.frames.last().unwrap().0;
                    assert_eq!(&done[..4], b"DONE");
                    assert_eq!(done.len(), 8);
                    assert!(u32::from_le_bytes(done[4..].try_into().unwrap()) > 0);
                    assert!(state.replies.is_empty());
                }
            }
        }
    }
    #[test]
    fn invalid_negotiated_sync_payload_fails_before_reading_or_writing() {
        for maximum in 0..=8 {
            let capture = Capture(Arc::default());
            let mut session = ADBSession::new(capture.clone(), 1, 2, maximum);
            assert!(session.push_file(io::empty()).is_err());
            assert!(capture.0.lock().unwrap().frames.is_empty());
        }
    }
}
