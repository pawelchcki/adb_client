use std::time::Duration;

/// Time limits for native ADB connections. `None` means no timeout.
///
/// Handshake limits apply to connection/authentication, including approval on
/// the device. Read/write limits apply to individual transport operations;
/// they are idle I/O limits, not a total running-time limit for a shell command.
#[derive(Clone, Copy, Debug)]
pub struct ADBTimeouts {
    /// Maximum time for the initial connection and ADB authentication.
    pub handshake: Option<Duration>,
    /// Maximum idle time while receiving a command's packets.
    pub read: Option<Duration>,
    /// Maximum idle time while sending a command's packets.
    pub write: Option<Duration>,
}

impl Default for ADBTimeouts {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl ADBTimeouts {
    /// Default native-device settings: unlimited reads and two-second writes.
    pub const DEFAULT: Self = Self {
        handshake: None,
        read: None,
        write: Some(Duration::from_secs(2)),
    };

    pub(crate) fn validate(self) -> crate::Result<Self> {
        if [self.handshake, self.read, self.write]
            .into_iter()
            .flatten()
            .any(|timeout| timeout.is_zero())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "timeouts must be positive, or None for no timeout",
            )
            .into());
        }
        Ok(self)
    }
}
