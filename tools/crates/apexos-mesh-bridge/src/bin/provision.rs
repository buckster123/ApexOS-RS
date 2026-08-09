//! `apexos-brainstem-provision` — the commissioning ceremony.
//!
//! Gives a brainstem its name (`node_id`) and the colony PSK, which it
//! persists to flash and keeps through its own power cycles and its Pi's
//! absence (charter §0.1, §5).
//!
//! ## Why this is a separate binary and not part of the bridge
//!
//! The bridge daemon is **PSK-free** — one of its four link laws. It runs
//! forever, parsing bytes from a device on the hostile edge of the system; if
//! it held the colony key, a bug in that parser would leak the whole colony's
//! key. This tool holds the key for the two seconds a commissioning takes and
//! then exits. Same key, vastly smaller exposure, and the law survives intact.
//!
//! ## Acceptance (enforced by the *board*, not by this tool)
//!
//! - A board with no stored key accepts an **unsealed** provision — trust on
//!   first use over a physical wire. Anyone who can reach the UART can already
//!   reflash the board, so refusing them protects nothing.
//! - A commissioned board accepts a provision **only if it opens under the key
//!   it already holds** (`--sealed`), which is what makes re-provisioning
//!   authenticated rather than a hijack.
//!
//! Full old-key→new-key rotation (dual-accept) is charter §8 / Phase 8.
//!
//! ## Usage
//!
//! ```text
//! # first touch — the board has never been commissioned
//! apexos-brainstem-provision --port /dev/ttyACM0 --node-id 1001
//!
//! # re-provision a board that already holds this colony's key
//! apexos-brainstem-provision --port /dev/ttyACM0 --node-id 1002 --sealed
//! ```
//!
//! Env: `APEXNET_PSK_FILE` (default `/etc/agentd/apexnet.psk`).
//!
//! **Stop the bridge first.** Both processes want the same serial device, and
//! two readers racing on one UART is how you get a "provisioning didn't take"
//! mystery.

use std::io::ErrorKind;
use std::process::ExitCode;
use std::time::Duration;

use apexos_mesh_proto::{
    encode_frame, seal, Deframer, MeshClass, Payload, PlainPacket, Psk, BROADCAST, WIRE_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::SerialPortBuilderExt;

const DEFAULT_PSK_PATH: &str = "/etc/agentd/apexnet.psk";
/// The cortex's own sender id on the wired link. Provisioning is the one
/// frame the Pi side originates, and it is point-to-point down a cable, so a
/// fixed id is honest — the router (P5b) owns real cortex identity.
const CORTEX_SENDER: u16 = 1;
/// How long to wait for the board to confirm via `BrainstemStatus`.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(12);

struct Args {
    port: String,
    baud: u32,
    node_id: u16,
    sealed: bool,
    /// Listen only: report what the board already believes, change nothing.
    status: bool,
    /// Hand the brainstem a message for another node, to carry and deliver.
    queue_to: Option<u16>,
    text: String,
}

fn usage() -> &'static str {
    "usage: apexos-brainstem-provision --port <dev> \
     (--node-id <n> [--sealed] | --status | --queue-to <n> --text <msg>) [--baud <rate>]"
}

fn parse_args() -> Result<Args, String> {
    let mut port = None;
    let mut node_id = None;
    let mut baud = 115_200u32;
    let mut sealed = false;
    let mut status = false;
    let mut queue_to = None;
    let mut text = String::new();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--port" => port = it.next(),
            "--node-id" => {
                node_id = Some(
                    it.next()
                        .ok_or("--node-id needs a value")?
                        .parse::<u16>()
                        .map_err(|e| format!("--node-id: {e}"))?,
                )
            }
            "--baud" => {
                baud = it
                    .next()
                    .ok_or("--baud needs a value")?
                    .parse()
                    .map_err(|e| format!("--baud: {e}"))?
            }
            "--sealed" => sealed = true,
            "--status" => status = true,
            "--queue-to" => {
                queue_to = Some(
                    it.next()
                        .ok_or("--queue-to needs a node id")?
                        .parse::<u16>()
                        .map_err(|e| format!("--queue-to: {e}"))?,
                )
            }
            "--text" => text = it.next().ok_or("--text needs a value")?,
            "-h" | "--help" => return Err(usage().into()),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }

    if let Some(dest) = queue_to {
        if text.is_empty() {
            return Err("--queue-to needs --text".into());
        }
        return Ok(Args {
            port: port.ok_or_else(|| format!("--port is required\n{}", usage()))?,
            baud,
            node_id: 0,
            sealed: false,
            status: false,
            queue_to: Some(dest),
            text,
        });
    }
    if status {
        return Ok(Args {
            port: port.ok_or_else(|| format!("--port is required\n{}", usage()))?,
            baud,
            node_id: 0,
            sealed: false,
            status: true,
            queue_to: None,
            text: String::new(),
        });
    }
    let node_id = node_id.ok_or_else(|| format!("--node-id is required\n{}", usage()))?;
    if node_id == 0 {
        // 0 is the firmware's "un-commissioned" marker; handing it out as a
        // real id would make a named board indistinguishable from a virgin one.
        return Err("--node-id 0 is reserved for un-commissioned boards".into());
    }
    Ok(Args {
        port: port.ok_or_else(|| format!("--port is required\n{}", usage()))?,
        baud,
        node_id,
        sealed,
        status: false,
        queue_to: None,
        text: String::new(),
    })
}

/// Read the 32-byte colony key. Accepts the hex form `install.sh` mints, and
/// raw 32 bytes, so a key file written by either path works.
fn load_psk(path: &str) -> Result<[u8; 32], String> {
    let raw = std::fs::read(path).map_err(|e| match e.kind() {
        ErrorKind::NotFound => format!(
            "no colony key at {path} — mint one with install.sh, or point \
             APEXNET_PSK_FILE at the node that has it"
        ),
        ErrorKind::PermissionDenied => {
            format!("cannot read {path} (it is root-owned by design — run with sudo)")
        }
        _ => format!("reading {path}: {e}"),
    })?;

    let text = String::from_utf8_lossy(&raw);
    let trimmed = text.trim();
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16)
                .map_err(|e| format!("{path} is not valid hex: {e}"))?;
        }
        return Ok(key);
    }
    <[u8; 32]>::try_from(raw.as_slice())
        .map_err(|_| format!("{path} is neither 64 hex chars nor 32 raw bytes"))
}

/// Hand a message down the wire addressed to ANOTHER node. The brainstem
/// stores it in flash and delivers it over the radio when that peer appears —
/// which is the point: the cortex can hand off and go away.
///
/// Unsealed, like everything on this wire: it is a cable between a board and
/// its own Pi, and the brainstem seals it for the air at delivery time with a
/// fresh counter.
async fn queue_message(args: &Args) -> ExitCode {
    let dest = args.queue_to.unwrap_or(0);
    let packet = PlainPacket {
        target: dest,
        hop_limit: apexos_mesh_proto::DEFAULT_HOP_LIMIT,
        flags: 0,
        payload: Payload::A2A {
            body: args.text.as_bytes().to_vec(),
        },
    };
    let ct = match postcard::to_allocvec(&packet) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("encoding message failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let frame = apexos_mesh_proto::MeshFrame {
        ver: WIRE_VERSION,
        class: MeshClass::Gossip,
        sender: CORTEX_SENDER,
        ctr: 1,
        ct,
    };
    let wire = match encode_frame(&frame) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("framing message failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut stream = match tokio_serial::new(&args.port, args.baud).open_native_async() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("opening {}: {e}", args.port);
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = stream.write_all(&wire).await {
        eprintln!("writing to {}: {e}", args.port);
        return ExitCode::FAILURE;
    }
    let _ = stream.flush().await;
    println!(
        "queued {} B for node {dest} — the board carries it from here",
        args.text.len()
    );
    ExitCode::SUCCESS
}

/// Peek at the board's current `node_id` from its own telemetry. `None` if it
/// says nothing in time — treated as "unknown", never as "un-commissioned".
async fn observe_node_id<S>(stream: &mut S) -> Option<u16>
where
    S: AsyncReadExt + Unpin,
{
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 512];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(7);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let read = match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => continue,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return None,
        };
        for got in deframer.push(&buf[..read]) {
            let Ok((packet, _)) = postcard::take_from_bytes::<PlainPacket>(&got.ct) else {
                continue;
            };
            if let Payload::BrainstemStatus { node_id, .. } = packet.payload {
                return Some(node_id);
            }
        }
    }
}

/// Listen for one `BrainstemStatus` and print it. Reads no key — asking a
/// board who it thinks it is should not require holding the colony's secret.
async fn report_status(args: &Args) -> ExitCode {
    let mut stream = match tokio_serial::new(&args.port, args.baud).open_native_async() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "opening {}: {e}\nis the bridge still running on this port?",
                args.port
            );
            return ExitCode::FAILURE;
        }
    };
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 512];
    let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            eprintln!("no status frame within {}s", CONFIRM_TIMEOUT.as_secs());
            return ExitCode::FAILURE;
        }
        let read = match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => continue,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                eprintln!("reading {}: {e}", args.port);
                return ExitCode::FAILURE;
            }
        };
        for got in deframer.push(&buf[..read]) {
            let Ok((packet, _)) = postcard::take_from_bytes::<PlainPacket>(&got.ct) else {
                continue;
            };
            if let Payload::BrainstemStatus {
                node_id,
                queued,
                neighbors,
                ctr_hw,
            } = packet.payload
            {
                let who = if node_id == 0 {
                    "UN-COMMISSIONED".to_string()
                } else {
                    format!("node_id={node_id}")
                };
                println!(
                    "{who} neighbors={neighbors} queued={queued} counter_high_water={ctr_hw}"
                );
                return ExitCode::SUCCESS;
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    if args.status {
        return report_status(&args).await;
    }
    if args.queue_to.is_some() {
        return queue_message(&args).await;
    }

    let psk_path =
        std::env::var("APEXNET_PSK_FILE").unwrap_or_else(|_| DEFAULT_PSK_PATH.to_string());
    let psk = match load_psk(&psk_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let packet = PlainPacket {
        target: BROADCAST,
        hop_limit: 1,
        flags: 0,
        payload: Payload::Provision {
            node_id: args.node_id,
            psk,
        },
    };

    // Unsealed for first touch; sealed proves key custody to a board that is
    // already commissioned.
    let frame = if args.sealed {
        match seal(
            &Psk(psk),
            MeshClass::Critical,
            CORTEX_SENDER,
            // The cortex's counter for this one-shot link. A board only ever
            // checks that the frame opens; the replay window that would make
            // this counter load-bearing belongs to the router (P5b).
            1,
            &packet,
        ) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("sealing provision failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        let ct = match postcard::to_allocvec(&packet) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("encoding provision failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        apexos_mesh_proto::MeshFrame {
            ver: WIRE_VERSION,
            class: MeshClass::Critical,
            sender: CORTEX_SENDER,
            ctr: 1,
            ct,
        }
    };

    let wire = match encode_frame(&frame) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("framing provision failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut stream = match tokio_serial::new(&args.port, args.baud).open_native_async() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "opening {}: {e}\nis the bridge still running on this port?",
                args.port
            );
            return ExitCode::FAILURE;
        }
    };

    // What does the board think it is BEFORE we touch it? Confirming on
    // "it reports the id we asked for" is vacuous when it already had that
    // id — the reply is byte-identical whether the provision was honoured or
    // refused, and a refusal that reads as success is worse than no check.
    let before = observe_node_id(&mut stream).await;
    if before == Some(args.node_id) {
        eprintln!(
            "note: this board already reports node_id={}, so its reply cannot\n\
             distinguish an accepted provision from a refused one. Use a\n\
             different --node-id if you need a confirmable result.",
            args.node_id
        );
    }

    if let Err(e) = stream.write_all(&wire).await {
        eprintln!("writing to {}: {e}", args.port);
        return ExitCode::FAILURE;
    }
    let _ = stream.flush().await;
    println!(
        "sent {} provision: node_id={} ({} B) — waiting for the board to confirm...",
        if args.sealed { "sealed" } else { "unsealed" },
        args.node_id,
        wire.len()
    );

    // Confirmation is the board's own telemetry: the next BrainstemStatus
    // carries the id it now believes it has. We assert on the board's word,
    // not on our write having succeeded.
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 512];
    let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            // Say which refusal is likely, rather than one guess for both
            // cases — a wrong-key rejection and a needs-sealing rejection look
            // identical on the wire (the board answers neither), so the hint
            // has to come from what we sent.
            let hint = if args.sealed {
                "The board refused a sealed provision: either it holds a \
                 different key than APEXNET_PSK_FILE, or it is not running \
                 provisioning-capable firmware."
            } else {
                "The board refused an unsealed provision, which means it is \
                 already commissioned — retry with --sealed to prove you hold \
                 its current key."
            };
            eprintln!(
                "no confirmation within {}s.\n{hint}",
                CONFIRM_TIMEOUT.as_secs()
            );
            return ExitCode::FAILURE;
        }
        let read = match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => continue,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                eprintln!("reading {}: {e}", args.port);
                return ExitCode::FAILURE;
            }
        };
        for got in deframer.push(&buf[..read]) {
            let Ok((packet, _)) = postcard::take_from_bytes::<PlainPacket>(&got.ct) else {
                continue;
            };
            // Edition 2021 here (the bridge crate) — no let-chains.
            if let Payload::BrainstemStatus {
                node_id, ctr_hw, ..
            } = packet.payload
            {
                if node_id == args.node_id {
                    if before == Some(args.node_id) {
                        println!(
                            "board reports node_id={node_id}, counter high-water {ctr_hw} \
                             (unchanged — see the note above; this is NOT proof it was accepted)"
                        );
                    } else {
                        println!(
                            "confirmed: board reports node_id={node_id}, counter high-water {ctr_hw}"
                        );
                    }
                    return ExitCode::SUCCESS;
                }
            }
        }
    }
}
