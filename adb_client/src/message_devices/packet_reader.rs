use crate::{
    BinaryDecodable, Result,
    message_devices::adb_transport_message::{
        ADBTransportMessage, ADBTransportMessageHeader, MAX_PAYLOAD_SIZE,
    },
};

/// Keep partial headers and payloads across idle timeouts. A timeout must never
/// cause the next call to parse the middle of a packet as a fresh header.
#[derive(Debug, Default)]
pub(crate) struct PacketReader {
    header: [u8; 24],
    header_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
}

impl PacketReader {
    pub(crate) fn read(
        &mut self,
        mut read: impl FnMut(&mut [u8]) -> Result<usize>,
    ) -> Result<ADBTransportMessage> {
        while self.header_read < self.header.len() {
            let size = read(&mut self.header[self.header_read..])?;
            if size == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.header_read += size;
        }
        let header = ADBTransportMessageHeader::decode(&self.header)?;
        if header.data_length() > MAX_PAYLOAD_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ADB packet exceeds the announced maximum payload",
            )
            .into());
        }
        self.payload.resize(header.data_length() as usize, 0);
        while self.payload_read < self.payload.len() {
            let size = read(&mut self.payload[self.payload_read..])?;
            if size == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.payload_read += size;
        }
        let message =
            ADBTransportMessage::from_header_and_payload(header, std::mem::take(&mut self.payload));
        self.header_read = 0;
        self.payload_read = 0;
        if !message.check_message_integrity() {
            let checksum = ADBTransportMessageHeader::compute_crc32(message.payload());
            if checksum != message.header().data_crc32() {
                return Err(crate::RustADBError::InvalidIntegrity(
                    checksum,
                    message.header().data_crc32(),
                ));
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid ADB packet checksum or magic",
            )
            .into());
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_devices::message_commands::MessageCommand;
    use std::io::{Error, ErrorKind};

    #[test]
    fn every_partial_header_and_payload_can_resume_after_timeout() {
        let original =
            ADBTransportMessage::try_new(MessageCommand::Write, 7, 9, b"\0\xffPNG\r\n\x1a\n")
                .unwrap();
        let mut bytes = original.header().as_bytes();
        bytes.extend_from_slice(original.payload());
        for split in 1..bytes.len() {
            let mut reader = PacketReader::default();
            let mut offset = 0;
            let err = reader
                .read(|out| {
                    if offset == split {
                        return Err(Error::from(ErrorKind::TimedOut).into());
                    }
                    let count = out.len().min(split - offset);
                    out[..count].copy_from_slice(&bytes[offset..offset + count]);
                    offset += count;
                    Ok(count)
                })
                .unwrap_err();
            assert!(
                matches!(err, crate::RustADBError::IOError(err) if err.kind() == ErrorKind::TimedOut)
            );
            let resumed = reader
                .read(|out| {
                    let count = out.len().min(bytes.len() - offset);
                    out[..count].copy_from_slice(&bytes[offset..offset + count]);
                    offset += count;
                    Ok(count)
                })
                .unwrap();
            assert_eq!(resumed.header().arg0(), 7);
            assert_eq!(resumed.header().arg1(), 9);
            assert_eq!(resumed.payload(), original.payload());
        }
    }
}
