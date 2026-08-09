//! Tier 2a: BLE gossip, driven straight over HCI.
//!
//! ## Why there is no BLE host stack here
//!
//! Gossip is **connectionless** — advertise and scan, no GATT, no
//! connections, no L2CAP. That is a handful of HCI commands and an event
//! loop. Bringing in a full BLE host to do it costs more code than it saves,
//! and the bench proved it costs correctness too: a host stack's reasonable
//! defaults (a hardcoded duplicate filter, a units bug in its command
//! encoding) are invisible from the outside and present exactly as dead
//! hardware. Everything this module sends is spelled out and testable.
//!
//! `docs/apexnet.md` §10 has the full account.
//!
//! ## The laws, each learned the expensive way
//!
//! 1. **Advertise NON-connectable** (`ADV_NONCONN_IND` / extended event
//!    properties `0x0000`). A connectable advertiser stops advertising the
//!    instant anything connects, and nothing re-arms it — a phone in the room
//!    silences the node permanently while it goes on receiving perfectly.
//! 2. **Scan with duplicate filtering OFF.** The controller's duplicate list
//!    never refreshes, so filtering means each neighbour is heard exactly
//!    once, ever.
//! 3. **Extended, not legacy.** A sealed heartbeat is ~46 B; legacy
//!    advertising has 27 usable. Extended carries 254.
//! 4. **Pair commands with responses by skipping async events.** Once
//!    scanning is live, advertising reports interleave with command
//!    completions; a naive one-read-per-command reads someone else's traffic
//!    and believes it is a status code.
//! 5. **Every length field is derived, never hand-counted.** An inconsistent
//!    one makes the controller drop the command silently, which desynchronises
//!    every later command/response pair.

use apexos_mesh_proto::{decode_datagram, encode_datagram, MeshFrame};
use esp_radio::ble::controller::BleConnector;

/// Company identifier used in the manufacturer-specific AD structure.
/// `0xFFFF` is the Bluetooth SIG's reserved-for-testing value — a private
/// colony protocol has no business squatting a real assignee's id.
const COMPANY_ID: [u8; 2] = [0xFF, 0xFF];
/// Marks the AD structure as ApexNET, so we ignore anything else that also
/// uses the reserved company id.
const MAGIC: [u8; 2] = *b"AX";
/// `type(1) + company(2) + magic(2)` — what the frame costs before its own
/// first byte.
const AD_OVERHEAD: usize = 5;
/// Largest frame that fits one extended advertisement.
pub const MAX_GOSSIP_FRAME: usize = 254 - AD_OVERHEAD - 1;

/// Advertising and scanning both at 100 ms, in the spec's 0.625 ms units.
/// (`bt-hci` 0.8.1 gets this wrong by 16x; we encode it ourselves.)
const INTERVAL_UNITS: u16 = 160;

/// What a scan turned up: an ApexNET frame and how loud it was.
pub struct Heard {
    pub frame: MeshFrame,
    pub rssi_dbm: i8,
}

/// Errors are deliberately coarse — the caller's only sane response to any of
/// them is to keep beating and try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioError {
    /// A command got no response, or a non-zero status.
    Command,
    /// The frame does not fit one extended advertisement.
    TooLarge,
}

/// The radio, as the rest of the firmware sees it: put a frame on the air,
/// take frames off it.
pub struct Radio<'d> {
    ble: BleConnector<'d>,
    /// One HCI packet: 3 B header + 255 B max payload, rounded up.
    buf: [u8; 260],
}

impl<'d> Radio<'d> {
    /// Bring the controller up and start scanning. Advertising begins with
    /// the first [`Radio::advertise`].
    pub async fn new(ble: BleConnector<'d>) -> Result<Self, RadioError> {
        let mut radio = Self {
            ble,
            buf: [0u8; 260],
        };

        // The controller needs a moment after `BleConnector::new` before it
        // answers HCI. Without this the very first command times out, `new`
        // fails closed, and the node simply never gossips — with nothing on
        // any wire to say why.
        embassy_time::Timer::after(embassy_time::Duration::from_millis(500)).await;

        // Reset, then unmask everything we care about. LE Meta (bit 61 of the
        // main mask) gates advertising reports; without it the controller is
        // correct, silent, and baffling.
        radio.command(&[0x01, 0x03, 0x0C, 0x00]).await?;
        radio
            .command(&[
                0x01, 0x01, 0x0C, 0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            ])
            .await?;
        radio
            .command(&[
                0x01, 0x01, 0x20, 0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            ])
            .await?;

        // LE Set Extended Scan Parameters (0x2041): public address, accept
        // all, LE 1M, passive.
        let iv = INTERVAL_UNITS.to_le_bytes();
        radio
            .command(&[
                0x01, 0x41, 0x20, 0x08, 0x00, 0x00, 0x01, 0x00, iv[0], iv[1], iv[0], iv[1],
            ])
            .await?;

        // LE Set Extended Advertising Parameters (0x2036). Event properties
        // 0x0000 = extended, NON-connectable, NON-scannable, undirected.
        radio
            .command(&[
                0x01, 0x36, 0x20, 0x19, 0x00, // adv handle
                0x00, 0x00, // event properties: the law above
                iv[0], iv[1], 0x00, // primary interval min
                iv[0], iv[1], 0x00, // primary interval max
                0x07, // all three primary channels
                0x00, // own address: public
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // peer (unused)
                0x00, // filter policy: accept all
                0x7F, // tx power: no host preference
                0x01, // primary PHY: LE 1M
                0x00, // secondary max skip
                0x01, // secondary PHY: LE 1M
                0x00, // advertising SID
                0x00, // no scan request notifications
            ])
            .await?;

        // LE Set Extended Scan Enable (0x2042). filter_duplicates = 0 — see
        // law 2; this single byte is the difference between a working
        // receiver and one that hears each neighbour once and never again.
        radio
            .command(&[0x01, 0x42, 0x20, 0x06, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00])
            .await?;

        Ok(radio)
    }

    /// Put a frame on the air, replacing whatever was advertising before.
    ///
    /// Advertising is a *standing* state, not a send: the controller repeats
    /// the current payload every interval until it is replaced. That is
    /// exactly the shape gossip wants — a heartbeat is a claim about now, and
    /// a listener that missed the last one gets the next.
    pub async fn advertise(&mut self, frame: &MeshFrame) -> Result<(), RadioError> {
        let body = encode_datagram(frame).map_err(|_| RadioError::TooLarge)?;
        if body.len() > MAX_GOSSIP_FRAME {
            return Err(RadioError::TooLarge);
        }

        // Derived lengths, never hand-counted (law 5).
        let ad_len = 1 + COMPANY_ID.len() + MAGIC.len() + body.len(); // type + content
        let adv_data_len = 1 + ad_len; // the AD structure on air
        let param_len = 4 + adv_data_len; // handle + op + frag + len + data

        let mut cmd = [0u8; 4 + 4 + 1 + AD_OVERHEAD + MAX_GOSSIP_FRAME];
        cmd[0] = 0x01;
        cmd[1] = 0x37;
        cmd[2] = 0x20;
        cmd[3] = param_len as u8;
        cmd[4] = 0x00; // adv handle
        cmd[5] = 0x03; // operation: complete
        cmd[6] = 0x01; // no fragmentation
        cmd[7] = adv_data_len as u8;
        cmd[8] = ad_len as u8;
        cmd[9] = 0xFF; // manufacturer specific
        cmd[10..12].copy_from_slice(&COMPANY_ID);
        cmd[12..14].copy_from_slice(&MAGIC);
        cmd[14..14 + body.len()].copy_from_slice(&body);
        let total = 4 + param_len;
        self.command(&cmd[..total]).await?;

        // LE Set Extended Advertising Enable (0x2039). Idempotent: re-enabling
        // an already-enabled set just keeps it running.
        self.command(&[0x01, 0x39, 0x20, 0x06, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00])
            .await
    }

    /// Wait for the next ApexNET frame on the air.
    ///
    /// Everything that is not an ApexNET advertising report is skipped
    /// silently: the 2.4 GHz band is full of other people's traffic, and this
    /// is a gossip receiver, not a sniffer.
    pub async fn next_frame(&mut self) -> Option<Heard> {
        loop {
            let n = match self.ble.read_async(&mut self.buf).await {
                Ok(n) if n > 0 => n,
                _ => return None,
            };
            if let Some(heard) = parse_ext_adv_report(&self.buf[..n]) {
                return Some(heard);
            }
        }
    }

    /// Send a command and wait for its completion, skipping the asynchronous
    /// events that interleave with it once scanning is live (law 4).
    async fn command(&mut self, cmd: &[u8]) -> Result<(), RadioError> {
        // Reuses the one owned buffer: a second scratch array here would
        // duplicate ~260 B inside every future that awaits a command.
        if self.ble.write(cmd).is_err() {
            return Err(RadioError::Command);
        }
        // Bounded: a controller that answers nothing must not wedge the
        // brainstem (principle 1 — it outlives the cortex, and the radio).
        for _ in 0..64 {
            let n = match embassy_time::with_timeout(
                embassy_time::Duration::from_millis(500),
                self.ble.read_async(&mut self.buf),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => n,
                _ => return Err(RadioError::Command),
            };
            // 0x0E Command Complete, 0x0F Command Status.
            if n >= 7 && self.buf[0] == 0x04 && self.buf[1] == 0x0E {
                return if self.buf[6] == 0x00 {
                    Ok(())
                } else {
                    Err(RadioError::Command)
                };
            }
            if n >= 4 && self.buf[0] == 0x04 && self.buf[1] == 0x0F {
                return if self.buf[3] == 0x00 {
                    Ok(())
                } else {
                    Err(RadioError::Command)
                };
            }
        }
        Err(RadioError::Command)
    }
}

/// Pull an ApexNET frame out of an HCI LE Extended Advertising Report.
///
/// Split out as a free function so it is readable in isolation: this is the
/// one place hostile bytes from the air become a typed frame, and every
/// length in it comes off the wire.
fn parse_ext_adv_report(pkt: &[u8]) -> Option<Heard> {
    // 04 3e <len> 0d <num_reports> then per report:
    // evt_type(2) addr_type(1) addr(6) pri_phy(1) sec_phy(1) sid(1)
    // tx_power(1) rssi(1) periodic_interval(2) direct_addr_type(1)
    // direct_addr(6) data_len(1) data(N)
    const HEADER: usize = 5;
    // evt_type(2) addr_type(1) addr(6) pri_phy(1) sec_phy(1) sid(1)
    // tx_power(1) rssi(1) periodic_interval(2) direct_addr_type(1)
    // direct_addr(6) = 23 bytes before data_len. Counted against a real
    // captured report, not from the spec prose — an off-by-one here reads the
    // AD type byte as a length and the whole walk collapses silently.
    const FIXED: usize = 23;
    if pkt.len() < HEADER + FIXED + 1 || pkt[0] != 0x04 || pkt[1] != 0x3E || pkt[3] != 0x0D {
        return None;
    }
    let rssi_dbm = pkt[HEADER + 13] as i8; // offset 13 within the report
    let data_len = pkt[HEADER + FIXED] as usize;
    let data_start = HEADER + FIXED + 1;
    let data = pkt.get(data_start..data_start + data_len)?;

    // Walk AD structures; ours is manufacturer-specific with our company id
    // and magic. Bounds come from the packet, so a truncated or lying length
    // ends the walk instead of reading past it.
    let mut i = 0usize;
    while i < data.len() {
        let len = data[i] as usize;
        if len == 0 || i + 1 + len > data.len() {
            break;
        }
        let ad_type = data[i + 1];
        let body = &data[i + 2..i + 1 + len];
        if ad_type == 0xFF
            && body.len() > COMPANY_ID.len() + MAGIC.len()
            && body[..2] == COMPANY_ID
            && body[2..4] == MAGIC
        {
            // Decode failures are silent by design: the air is hostile, and a
            // malformed frame is not an event worth spending a log line on.
            if let Ok(frame) = decode_datagram(&body[4..]) {
                return Some(Heard { frame, rssi_dbm });
            }
        }
        i += 1 + len;
    }
    None
}
