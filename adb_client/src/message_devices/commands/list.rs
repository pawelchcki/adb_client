use crate::{
    Result,
    message_devices::{
        adb_message_device::ADBMessageDevice,
        adb_message_transport::ADBMessageTransport,
        adb_transport_message::ADBTransportMessage,
        message_commands::{MessageCommand, MessageSubcommand},
        utils::BinaryEncodable,
    },
    models::ADBListItemType,
};

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    /// List directory entries using the legacy sync byte stream.
    pub(crate) fn list<A: AsRef<str>>(&mut self, path: A) -> Result<Vec<ADBListItemType>> {
        let mut session = self.open_synchronization_session()?;
        let path = path.as_ref();
        let mut request = MessageSubcommand::List
            .with_arg(u32::try_from(path.len())?)
            .encode();
        request.extend_from_slice(path.as_bytes());
        session.send_and_expect_okay(ADBTransportMessage::try_new(
            MessageCommand::Write,
            session.local_id(),
            session.remote_id(),
            &request,
        )?)?;
        let entries = session.recv_list()?;
        self.end_transaction(&mut session)?;
        Ok(entries)
    }
}
