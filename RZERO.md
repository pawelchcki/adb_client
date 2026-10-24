# rzero dependency patches

This branch builds on cocool97/adb_client commit
`7728265d2b36c9ec2fcece638a59d4d90ab28fba` and preserves its negotiated
payload limits and coalesced sync uploads. rzero pins a full Git revision of
this fork instead of keeping a vendored copy.

The portable performance patch reuses coalesced upload buffers, borrows message
writes, encodes fixed headers on the stack, and uses vectored TCP output with
EOF and partial-write handling. USB packet splitting is already handled by
upstream's whole-buffer libusb transfers; partial transfers and ZLPs remain.

Additional compatibility patches preserve rzero's imported-key authentication:
strict constructors never generate a key, send AUTH_RSAPUBLICKEY, or fall back
to TLS pairing. A separate explicit enrollment constructor uses the same
existing key with bounded handshake reads. Original constructors retain their
upstream authentication policy and payload negotiation.

Sync readers handle fragmented/coalesced DATA, DONE, FAIL and directory frames
with bounded allocations and length validation. USB discovery and endpoint
selection share the same recognized interface predicate. Unit tests cover
these contracts, and disposable QEMU composite USB/TCP fixtures exercise both
the Rust library and Android platform-tools. No credentials or device captures
are included in the source.
