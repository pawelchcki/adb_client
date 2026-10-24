use std::{
    io::{Cursor, Read},
    time::{Duration, SystemTime},
};

use crate::{
    ADBListItem, ADBListItemType, AdbStatResponse, BinaryDecodable, Result, RustADBError,
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

const LIST_ENTRIES_MAX: usize = 100_000;
const LIST_NAME_BYTES_MAX: usize = 16 * 1024 * 1024;

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
        let mut payload = Cursor::new(Vec::new());
        loop {
            let mut header = [0; 8];
            self.read_sync_exact(&mut payload, &mut header)?;
            let length = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
            match &header[..4] {
                b"DONE" if length == 0 => return Ok(()),
                b"DATA" if length <= SYNC_DATA_MAX_SIZE => {
                    // Write directly from the received WRTE, including when a
                    // DATA frame spans messages or shares one with other frames.
                    let mut left = length;
                    while left != 0 {
                        self.ensure_sync_payload(&mut payload)?;
                        let start = payload.position() as usize;
                        let count = left.min(payload.get_ref().len() - start);
                        output.write_all(&payload.get_ref()[start..start + count])?;
                        payload.set_position((start + count) as u64);
                        left -= count;
                    }
                }
                b"FAIL" if length <= SYNC_DATA_MAX_SIZE => {
                    let mut error = vec![0; length];
                    self.read_sync_exact(&mut payload, &mut error)?;
                    return Err(RustADBError::ADBRequestFailed(format!(
                        "ADB file read failed: {}",
                        String::from_utf8_lossy(&error)
                    )));
                }
                _ => {
                    return Err(RustADBError::ADBRequestFailed(
                        "Invalid or oversized ADB sync file response".into(),
                    ));
                }
            }
        }
    }

    pub(crate) fn recv_list(&mut self) -> Result<Vec<ADBListItemType>> {
        self.recv_list_bounded(LIST_ENTRIES_MAX, LIST_NAME_BYTES_MAX)
    }

    fn recv_list_bounded(
        &mut self,
        max_entries: usize,
        max_name_bytes: usize,
    ) -> Result<Vec<ADBListItemType>> {
        let mut payload = Cursor::new(Vec::new());
        let mut entries = Vec::new();
        let mut name_bytes = 0;
        loop {
            let mut status = [0; 4];
            self.read_sync_exact(&mut payload, &mut status)?;
            match &status {
                b"DENT" | b"DONE" => {
                    let mut metadata = [0; 16];
                    self.read_sync_exact(&mut payload, &mut metadata)?;
                    if &status == b"DONE" {
                        return Ok(entries);
                    }
                    let number = |offset| {
                        u32::from_le_bytes(metadata[offset..offset + 4].try_into().unwrap())
                    };
                    let mode = number(0);
                    let length = number(12) as usize;
                    if length == 0 || length > SYNC_DATA_MAX_SIZE {
                        return Err(RustADBError::ADBRequestFailed(
                            "Invalid or oversized ADB directory entry name".into(),
                        ));
                    }
                    name_bytes += length;
                    if entries.len() >= max_entries || name_bytes > max_name_bytes {
                        return Err(RustADBError::ADBRequestFailed(format!(
                            "ADB directory listing exceeds {max_entries} entries or {max_name_bytes} name bytes"
                        )));
                    }
                    let mut name = vec![0; length];
                    self.read_sync_exact(&mut payload, &mut name)?;
                    entries.push(ADBListItemType::from_mode_and_entry(
                        mode,
                        ADBListItem {
                            name: String::from_utf8(name)?,
                            time: number(8),
                            permissions: mode & 0o777,
                            size: number(4),
                        },
                    ));
                }
                b"FAIL" => {
                    let mut length = [0; 4];
                    self.read_sync_exact(&mut payload, &mut length)?;
                    let length = u32::from_le_bytes(length) as usize;
                    if length > SYNC_DATA_MAX_SIZE {
                        return Err(RustADBError::ADBRequestFailed(
                            "Oversized ADB directory error".into(),
                        ));
                    }
                    let mut error = vec![0; length];
                    self.read_sync_exact(&mut payload, &mut error)?;
                    return Err(RustADBError::ADBRequestFailed(format!(
                        "ADB directory read failed: {}",
                        String::from_utf8_lossy(&error)
                    )));
                }
                _ => {
                    return Err(RustADBError::ADBRequestFailed(
                        "Invalid ADB directory response".into(),
                    ));
                }
            }
        }
    }

    // Sync is a byte stream: both headers and data may span WRTE messages,
    // and a single WRTE may contain multiple complete sync frames.
    fn read_sync_exact(
        &mut self,
        payload: &mut Cursor<Vec<u8>>,
        mut output: &mut [u8],
    ) -> Result<()> {
        while !output.is_empty() {
            self.ensure_sync_payload(payload)?;
            let count = payload.read(output)?;
            output = &mut output[count..];
        }
        Ok(())
    }

    fn ensure_sync_payload(&mut self, payload: &mut Cursor<Vec<u8>>) -> Result<()> {
        if payload.position() == payload.get_ref().len() as u64 {
            let message = self
                .transport
                .read_message_with_timeout(Duration::from_secs(30))?;
            message.assert_command(MessageCommand::Write)?;
            let bytes = message.into_payload();
            if bytes.is_empty() {
                return Err(RustADBError::ADBRequestFailed(
                    "Empty ADB sync payload before file completion".into(),
                ));
            }
            self.transport.write_message(ADBTransportMessage::try_new(
                MessageCommand::Okay,
                self.local_id,
                self.remote_id,
                &[],
            )?)?;
            *payload = Cursor::new(bytes);
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
        response.assert_command(MessageCommand::Write)?;
        // Skip first 4 bytes as this is the literal "STAT".
        // Interesting part starts right after

        let payload = response.into_payload();
        if payload.get(..4) != Some(b"STAT") {
            return Err(RustADBError::ADBRequestFailed(
                "Invalid ADB STAT response".into(),
            ));
        }
        AdbStatResponse::decode(&payload[4..])
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

#[cfg(test)]
mod recv_tests {
    use super::*;
    use crate::adb_transport::ADBTransport;
    use std::{
        collections::VecDeque,
        io,
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Debug)]
    struct Fake(Arc<Mutex<VecDeque<ADBTransportMessage>>>);
    impl ADBTransport for Fake {
        fn connect(&mut self) -> Result<()> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }
    impl ADBMessageTransport for Fake {
        fn read_message_with_timeout(&mut self, _: Duration) -> Result<ADBTransportMessage> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof).into())
        }
        fn write_message_with_timeout(
            &mut self,
            message: ADBTransportMessage,
            _: Duration,
        ) -> Result<()> {
            assert!(matches!(
                message.header().command(),
                MessageCommand::Okay | MessageCommand::Write
            ));
            Ok(())
        }
    }
    fn messages(messages: impl IntoIterator<Item = ADBTransportMessage>) -> ADBSession<Fake> {
        ADBSession::new(
            Fake(Arc::new(Mutex::new(messages.into_iter().collect()))),
            1,
            2,
            1048576,
        )
    }
    fn fragmented(bytes: &[u8], size: usize) -> ADBSession<Fake> {
        messages(
            bytes.chunks(size).map(|chunk| {
                ADBTransportMessage::try_new(MessageCommand::Write, 2, 1, chunk).unwrap()
            }),
        )
    }
    fn frame(id: &[u8; 4], bytes: &[u8]) -> Vec<u8> {
        [id.as_slice(), &(bytes.len() as u32).to_le_bytes(), bytes].concat()
    }
    fn dent(mode: u32, size: u32, time: u32, name: &str) -> Vec<u8> {
        [
            b"DENT".as_slice(),
            &mode.to_le_bytes(),
            &size.to_le_bytes(),
            &time.to_le_bytes(),
            &(name.len() as u32).to_le_bytes(),
            name.as_bytes(),
        ]
        .concat()
    }
    fn list_done() -> Vec<u8> {
        [b"DONE".as_slice(), &[0; 16]].concat()
    }
    #[test]
    fn directory_metadata_and_long_names_span_arbitrary_usb_messages() {
        let name = "photo-".repeat(32);
        let bytes = [
            dent(0o040755, 0, 42, "."),
            dent(0o100640, 123, 456, &name),
            list_done(),
        ]
        .concat();
        for size in [1, 2, 5, 19, 64, bytes.len()] {
            let mut session = fragmented(&bytes, size);
            let entries = session.recv_list().unwrap();
            assert_eq!(
                entries,
                vec![
                    ADBListItemType::Directory(ADBListItem {
                        name: ".".into(),
                        time: 42,
                        permissions: 0o755,
                        size: 0
                    }),
                    ADBListItemType::File(ADBListItem {
                        name: name.clone(),
                        time: 456,
                        permissions: 0o640,
                        size: 123
                    }),
                ]
            );
            assert!(session.transport.0.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn directory_truncation_and_invalid_lengths_return_errors() {
        let bytes = [dent(0o100644, 123, 456, "photo.jpg"), list_done()].concat();
        for end in 0..bytes.len() {
            assert!(fragmented(&bytes[..end], 1).recv_list().is_err());
        }
        let mut oversized = dent(0o100644, 0, 0, "x");
        oversized[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(fragmented(&oversized, 2).recv_list().is_err());
        assert!(fragmented(b"BOOM", 1).recv_list().is_err());
    }
    #[test]
    fn directory_listings_are_bounded_by_entries_and_name_bytes() {
        let bytes = [
            dent(0o100644, 0, 0, "a"),
            dent(0o100644, 0, 0, "bb"),
            list_done(),
        ]
        .concat();
        assert_eq!(
            fragmented(&bytes, 3).recv_list_bounded(2, 3).unwrap().len(),
            2
        );
        assert!(fragmented(&bytes, 3).recv_list_bounded(1, 3).is_err());
        assert!(fragmented(&bytes, 3).recv_list_bounded(2, 2).is_err());
    }
    #[test]
    fn directory_failure_and_empty_done_are_distinct_protocol_results() {
        let failure = frame(b"FAIL", b"Permission denied");
        assert!(
            fragmented(&failure, 1)
                .recv_list()
                .unwrap_err()
                .to_string()
                .contains("Permission denied")
        );
        assert!(fragmented(&list_done(), 1).recv_list().unwrap().is_empty());
    }
    #[test]
    fn reads_fragmented_and_coalesced_frames_without_treating_file_bytes_as_done() {
        let content = b"photo-DONE\0\0\0\0-still-photo";
        let bytes = [
            frame(b"DATA", content),
            frame(b"DATA", b"tail"),
            frame(b"DONE", b""),
        ]
        .concat();
        for size in [1, 3, 7, 8, 13, bytes.len()] {
            let mut output = Vec::new();
            fragmented(&bytes, size).recv_file(&mut output).unwrap();
            assert_eq!(output, [content.as_slice(), b"tail"].concat());
        }
        let mut output = Vec::new();
        fragmented(&frame(b"DONE", b""), 1)
            .recv_file(&mut output)
            .unwrap();
        assert!(output.is_empty());
    }
    #[test]
    fn fail_frames_report_errors_without_writing_error_messages_into_the_file() {
        let bytes = [
            frame(b"DATA", b"partial"),
            frame(b"FAIL", b"Permission denied"),
        ]
        .concat();
        for size in [1, 5, bytes.len()] {
            let mut output = Vec::new();
            let error = fragmented(&bytes, size).recv_file(&mut output).unwrap_err();
            assert!(error.to_string().contains("Permission denied"));
            assert_eq!(output, b"partial");
        }
    }
    #[test]
    fn truncated_invalid_and_oversized_responses_return_errors_instead_of_panicking() {
        let bytes = [frame(b"DATA", b"abc"), frame(b"DONE", b"")].concat();
        for end in 0..bytes.len() {
            assert!(fragmented(&bytes[..end], 1).recv_file(Vec::new()).is_err());
        }
        for id in [b"FAIL", b"DATA", b"BOOM"] {
            let bytes = [id.as_slice(), &u32::MAX.to_le_bytes()].concat();
            assert!(fragmented(&bytes, 3).recv_file(Vec::new()).is_err());
        }
        let close = ADBTransportMessage::try_new(MessageCommand::Clse, 2, 1, &[]).unwrap();
        assert!(messages([close]).recv_file(Vec::new()).is_err());
    }
    #[test]
    fn local_write_errors_are_propagated_for_partial_file_cleanup() {
        struct Full;
        impl io::Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes = [frame(b"DATA", b"abc"), frame(b"DONE", b"")].concat();
        assert!(
            fragmented(&bytes, 3)
                .recv_file(Full)
                .unwrap_err()
                .to_string()
                .contains("disk full")
        );
    }
    #[test]
    fn truncated_stat_responses_return_errors_before_slicing() {
        for bytes in [b"".as_slice(), b"S", b"STAT"] {
            let okay = || ADBTransportMessage::try_new(MessageCommand::Okay, 2, 1, &[]).unwrap();
            let reply = ADBTransportMessage::try_new(MessageCommand::Write, 2, 1, bytes).unwrap();
            assert!(
                messages([okay(), okay(), reply])
                    .stat_with_explicit_ids("/photo.jpg")
                    .is_err()
            );
        }
    }
}
