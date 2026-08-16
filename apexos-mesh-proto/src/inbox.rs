//! Radio inbox: hold an authenticated addressed packet until the cortex
//! durably accepts it (SA-2).
//!
//! The brainstem used to ACK on the air as soon as `try_send` to the USB
//! queue returned — including when that send dropped the frame, and even
//! when agentd ignored every payload except status. The sender then retired
//! data the Pi never had.
//!
//! Law: persist `(sender, ctr, packet)` first, ACK on the air only after the
//! host says it accepted that pair. Replay of a held pair waits; replay of a
//! pair already taken is a re-ACK (the radio ACK may have been lost).

use crate::crypto::ReplayAdmit;
use crate::crypto::ReplayTable;

/// How many addressed inbound packets the brainstem will hold waiting for
/// the Pi. Matches the USB TX queue depth — more would sit in flash with
/// nowhere to go.
pub const MAX_INBOX: usize = 8;

/// Same ceiling as the brainstem outbox: a radio frame cannot exceed one
/// extended advertisement.
pub const INBOX_PACKET_MAX: usize = 256;

/// Packed flash record: `sender_le(2) ++ ctr_le(8) ++ len_le(2) ++ bytes`.
pub const INBOX_SLOT_BYTES: usize = 2 + 8 + 2 + INBOX_PACKET_MAX;

/// One held inbound packet, still waiting for a host accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboxSlot {
    pub sender: u16,
    pub ctr: u64,
    pub len: u16,
    pub bytes: [u8; INBOX_PACKET_MAX],
}

impl InboxSlot {
    pub fn packet(&self) -> &[u8] {
        let n = self.len as usize;
        &self.bytes[..n.min(INBOX_PACKET_MAX)]
    }
}

/// Never-evict inbox. Full ⇒ refuse a *new* pair (the sender has not been
/// ACK'd, so it will retry). A duplicate insert is a no-op.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InboxTable {
    slots: [Option<InboxSlot>; MAX_INBOX],
}

impl InboxTable {
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_INBOX],
        }
    }

    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&self) -> bool {
        self.len() == MAX_INBOX
    }

    pub fn contains(&self, sender: u16, ctr: u64) -> bool {
        self.slots.iter().any(|s| {
            matches!(s, Some(e) if e.sender == sender && e.ctr == ctr)
        })
    }

    /// Insert or ignore a duplicate. `Err` if the packet is too large or
    /// the table is full of *other* pairs.
    pub fn insert(&mut self, sender: u16, ctr: u64, packet: &[u8]) -> Result<(), InboxRefuse> {
        if packet.len() > INBOX_PACKET_MAX {
            return Err(InboxRefuse::TooLarge);
        }
        if self.contains(sender, ctr) {
            return Ok(());
        }
        let Some(free) = self.slots.iter_mut().find(|s| s.is_none()) else {
            return Err(InboxRefuse::Full);
        };
        let mut bytes = [0u8; INBOX_PACKET_MAX];
        bytes[..packet.len()].copy_from_slice(packet);
        *free = Some(InboxSlot {
            sender,
            ctr,
            len: packet.len() as u16,
            bytes,
        });
        Ok(())
    }

    pub fn take(&mut self, sender: u16, ctr: u64) -> Option<InboxSlot> {
        for slot in &mut self.slots {
            if matches!(slot, Some(e) if e.sender == sender && e.ctr == ctr) {
                return slot.take();
            }
        }
        None
    }

    pub fn iter(&self) -> impl Iterator<Item = &InboxSlot> {
        self.slots.iter().filter_map(|s| s.as_ref())
    }

    /// Always a record, including a zeroed tombstone for a vacant slot so a
    /// take() can erase the flash copy (replay never removes slots; this does).
    pub fn encode_slot(&self, i: usize) -> Option<[u8; INBOX_SLOT_BYTES]> {
        let slot = self.slots.get(i)?;
        Some(match slot {
            Some(s) => encode_inbox_slot(s),
            None => [0u8; INBOX_SLOT_BYTES],
        })
    }

    pub fn load_slot(&mut self, bytes: &[u8]) -> bool {
        let Some(slot) = decode_inbox_slot(bytes) else {
            return false;
        };
        if slot.len == 0 {
            return true;
        }
        self.insert(slot.sender, slot.ctr, slot.packet()).is_ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxRefuse {
    Full,
    TooLarge,
}

/// What the radio loop should do with one authenticated inbound frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioInbound {
    /// Fresh + held: forward to the USB and wait for the host.
    Deliver,
    /// Already in the inbox: do not ACK, keep retrying USB.
    WaitHost,
    /// Replay of a pair we already handed to the host: ACK again.
    ReAck,
    /// Cannot hold it (inbox full, packet too large, replay table full).
    /// Do not ACK — the sender still has the message.
    Drop,
}

/// Decide *and* update replay/inbox. Inbox space is checked before the
/// replay window advances so a full table cannot eat a counter we then
/// refuse to hold.
pub fn decide_radio_inbound(
    replay: &mut ReplayTable,
    inbox: &mut InboxTable,
    sender: u16,
    ctr: u64,
    packet: &[u8],
) -> RadioInbound {
    if inbox.contains(sender, ctr) {
        return RadioInbound::WaitHost;
    }
    if packet.len() > INBOX_PACKET_MAX || inbox.is_full() {
        return RadioInbound::Drop;
    }
    match replay.accept(sender, ctr) {
        ReplayAdmit::Fresh => {
            let _ = inbox.insert(sender, ctr, packet);
            RadioInbound::Deliver
        }
        ReplayAdmit::Replay => RadioInbound::ReAck,
        ReplayAdmit::TableFull => RadioInbound::Drop,
    }
}

pub fn encode_inbox_slot(slot: &InboxSlot) -> [u8; INBOX_SLOT_BYTES] {
    let mut b = [0u8; INBOX_SLOT_BYTES];
    b[0..2].copy_from_slice(&slot.sender.to_le_bytes());
    b[2..10].copy_from_slice(&slot.ctr.to_le_bytes());
    b[10..12].copy_from_slice(&slot.len.to_le_bytes());
    let n = slot.len as usize;
    b[12..12 + n].copy_from_slice(&slot.bytes[..n]);
    b
}

pub fn decode_inbox_slot(bytes: &[u8]) -> Option<InboxSlot> {
    if bytes.len() < 12 {
        return None;
    }
    let sender = u16::from_le_bytes(bytes[0..2].try_into().ok()?);
    let ctr = u64::from_le_bytes(bytes[2..10].try_into().ok()?);
    let len = u16::from_le_bytes(bytes[10..12].try_into().ok()?);
    if len as usize > INBOX_PACKET_MAX {
        return None;
    }
    if bytes.len() < 12 + len as usize {
        return None;
    }
    let mut slot_bytes = [0u8; INBOX_PACKET_MAX];
    slot_bytes[..len as usize].copy_from_slice(&bytes[12..12 + len as usize]);
    Some(InboxSlot {
        sender,
        ctr,
        len,
        bytes: slot_bytes,
    })
}
