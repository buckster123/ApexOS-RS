use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use apexos_confine::{Access, Denied};
use std::process::Command;
use std::time::Duration;

// ─── Tool list ───────────────────────────────────────────────────────────────

pub fn list() -> Value {
    json!([
        {
            "name": "run_command",
            "description": "Execute a shell command. A best-effort denylist rejects the most obvious destructive operations, but it is heuristic and bypassable — the real guard is the approval gate (run_command is policy 'ask').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "Command to run (passed to /bin/sh -c)" },
                    "cwd": { "type": "string", "description": "Working directory (optional)" },
                    "env": { "type": "object", "description": "Extra environment variables (optional)", "additionalProperties": { "type": "string" } },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 30, max 300)" }
                },
                "required": ["cmd"]
            }
        },
        {
            "name": "read_file",
            "description": "Read a file from the filesystem.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "max_bytes": { "type": "integer", "description": "Maximum bytes to read (default 1MB)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "write_file",
            "description": "Write or append to a file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "append": { "type": "boolean", "description": "Append instead of overwrite (default false)" }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "list_dir",
            "description": "List directory contents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "recursive": { "type": "boolean", "description": "Recurse into subdirectories (max 3 levels)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "create_dir",
            "description": "Create a directory (and parents).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "delete_path",
            "description": "Delete a file or directory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "recursive": { "type": "boolean", "description": "Required true to delete a directory" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "notes_list",
            "description": "List the user's notes (markdown files in the shared notebook). Returns each note's name and size.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "notes_read",
            "description": "Read one of the user's notes from the shared notebook by name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Note name, e.g. 'groceries' or 'groceries.md'" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "notes_append",
            "description": "Append a line of text to one of the user's notes in the shared notebook, creating the note if it doesn't exist. Use this to leave the user a note.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Note name, e.g. 'ideas' or 'ideas.md'" },
                    "text": { "type": "string", "description": "Text to append (a trailing newline is added)" }
                },
                "required": ["name", "text"]
            }
        },
        {
            "name": "sketch_snapshot",
            "description": "Get the path to the latest drawing from the user's Sketchpad (a PNG under the workspace). Use this when the user says they drew something or asks you to look at their sketch, then view/describe the returned image path.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "sketch_draw",
            "description": "Draw on the Sketchpad canvas — your hand on this device's screen. Use it to sketch, diagram, or doodle for the user, or to add to what they've drawn. Coordinates are normalized 0.0-1.0 with the origin at the TOP-LEFT (x increases right, y increases down), so they scale to any canvas size. Provide `strokes`, drawn in order: each stroke is EITHER a freehand path (`points`: a list of [x,y] pairs) OR a primitive (`shape`: \"line\"|\"rect\"|\"ellipse\", with `from` and `to` corner points). Optional per-stroke `color` (\"#rrggbb\", default off-white) and `width` (px, default 3). Set `clear: true` to wipe the canvas first. The drawing appears live on the canvas; call sketch_snapshot afterwards to SEE the composited result. Works on the Slint UI (kiosk/desktop); a no-op (not an error) on a headless node.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "clear":   { "type": "boolean", "description": "Wipe the canvas before drawing (default false)" },
                    "strokes": {
                        "type": "array",
                        "description": "Strokes to draw, in order. Each needs either `points` or `shape`+`from`+`to`.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "points": { "type": "array", "description": "Freehand path: list of [x,y] pairs, each 0.0-1.0", "items": { "type": "array", "items": { "type": "number" } } },
                                "shape":  { "type": "string", "description": "Primitive: line|rect|ellipse (use with from/to instead of points)" },
                                "from":   { "type": "array", "description": "[x,y] start (line) or a corner (rect/ellipse), 0.0-1.0", "items": { "type": "number" } },
                                "to":     { "type": "array", "description": "[x,y] end (line) or the opposite corner (rect/ellipse), 0.0-1.0", "items": { "type": "number" } },
                                "color":  { "type": "string", "description": "Stroke colour #rrggbb (default #e6e6eb)" },
                                "width":  { "type": "number", "description": "Stroke width in px (default 3)" }
                            }
                        }
                    }
                },
                "required": ["strokes"]
            }
        },
        {
            "name": "screenshot_mirror",
            "description": "Capture and SEE your own live screen — the ApexOS-RS UI as it is rendered right now. Use this to look in the mirror: inspect the current interface, verify a UI change you just made, or check what the user is looking at. Returns the screenshot inline so you see it directly. Only works on a node with a display (kiosk or desktop); returns a note when headless.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "camera_capture",
            "description": "SEE the physical world through this device's camera — take a photo and look at it. Use this when the user asks what you see, to look at something they're holding up, check the room, or read a label/screen in front of the camera. Auto-detects the camera backend (Raspberry Pi CSI camera via rpicam/libcamera, or a USB/laptop webcam via V4L2), captures one frame, and returns it inline so you see it directly. Returns a note (not an error) when no camera is attached.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": { "type": "string", "description": "Optional V4L2 device path to force, e.g. '/dev/video0'. Omit to auto-detect (Pi CSI camera first, then the first USB/webcam node)." }
                }
            }
        },
        {
            "name": "http_fetch",
            "description": "Make an HTTP request.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "method": { "type": "string", "description": "HTTP method (default GET)" },
                    "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                    "body": { "type": "string" }
                },
                "required": ["url"]
            }
        },
        {
            "name": "cpu_temp",
            "description": "Read CPU temperature from thermal sensors.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "disk_usage",
            "description": "Report disk usage for mounted filesystems.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Filter to filesystem containing this path (optional)" }
                }
            }
        },
        {
            "name": "memory_info",
            "description": "Report system memory usage from /proc/meminfo.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "uptime",
            "description": "Report system uptime and load averages.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "notify",
            "description": "Send a notification across all available surfaces: JSONL log (always), notify-send toast (best-effort), TTS via espeak-ng or piper (if PIPER_MODEL env set), ntfy.sh push (if NTFY_TOPIC env set), Telegram (if TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID env set).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "Notification body — spoken aloud if TTS is available" },
                    "title":   { "type": "string", "description": "Title for toast and push surfaces (default: ApexOS)" },
                    "tts":     { "type": "boolean", "description": "Enable TTS (default true)" }
                },
                "required": ["message"]
            }
        },
        {
            "name": "audio_analyze",
            "description": "Analyze an audio file: duration, LUFS loudness, peak dB, RMS, silence at start/end, clipping, DC offset. Uses ffprobe + ffmpeg. No modification — read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to audio file (.mp3, .wav, .flac, etc.)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "audio_trim_silence",
            "description": "Remove silence from the start and/or end of an audio file using ffmpeg silenceremove filter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":              { "type": "string", "description": "Input audio file path" },
                    "output_path":       { "type": "string", "description": "Output file path" },
                    "start":             { "type": "boolean", "description": "Trim silence from start (default true)" },
                    "end":               { "type": "boolean", "description": "Trim silence from end (default true)" },
                    "threshold_db":      { "type": "number", "description": "Silence threshold in dB (default -50)" },
                    "min_silence_ms":    { "type": "integer", "description": "Minimum silence duration to remove in ms (default 500)" }
                },
                "required": ["path", "output_path"]
            }
        },
        {
            "name": "audio_normalize",
            "description": "Normalize audio loudness to a target LUFS using two-pass ffmpeg loudnorm. Accurate integrated loudness correction.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":         { "type": "string", "description": "Input audio file path" },
                    "output_path":  { "type": "string", "description": "Output file path" },
                    "target_lufs":  { "type": "number", "description": "Target integrated loudness in LUFS (default -14)" },
                    "true_peak":    { "type": "number", "description": "Max true peak in dBTP (default -2.0)" }
                },
                "required": ["path", "output_path"]
            }
        },
        {
            "name": "audio_peak_limit",
            "description": "Apply a true-peak limiter to prevent clipping. Uses ffmpeg alimiter filter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":         { "type": "string", "description": "Input audio file path" },
                    "output_path":  { "type": "string", "description": "Output file path" },
                    "limit_db":     { "type": "number", "description": "Peak limit in dB (default -1.0)" }
                },
                "required": ["path", "output_path"]
            }
        },
        {
            "name": "audio_trim",
            "description": "Trim an audio file to a specific time range using ffmpeg stream copy (fast, no re-encode).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":         { "type": "string", "description": "Input audio file path" },
                    "output_path":  { "type": "string", "description": "Output file path" },
                    "start_s":      { "type": "number", "description": "Start time in seconds (default 0)" },
                    "end_s":        { "type": "number", "description": "End time in seconds (required)" }
                },
                "required": ["path", "output_path", "end_s"]
            }
        },
        {
            "name": "audio_clean",
            "description": "One-shot composite audio fix: analyzes then applies trim_silence, normalize, and/or peak_limit as needed. Ideal post-processing after downloading a Sonus track.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":                  { "type": "string", "description": "Input audio file path" },
                    "output_path":           { "type": "string", "description": "Output file path (default: <name>_clean.<ext>)" },
                    "target_lufs":           { "type": "number", "description": "Target integrated loudness (default -14)" },
                    "silence_threshold_db":  { "type": "number", "description": "Silence detection threshold (default -50)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "gpio_info",
            "description": "Report GPIO hardware info: Pi model, chip path (gpiochip4 on Pi 5, gpiochip0 on Pi 3/4), sysfs base, reserved pins, and availability of gpioget/gpioset tools.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "gpio_read",
            "description": "Read a GPIO pin state (0=low, 1=high). Pi GPIO is 3.3V logic. Uses gpioget from libgpiod. Refuses reserved pins (I2C: 2,3; HAT EEPROM: 27,28).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "gpio": { "type": "integer", "description": "BCM GPIO number (0-27)" }
                },
                "required": ["gpio"]
            }
        },
        {
            "name": "gpio_write",
            "description": "Set a GPIO pin high (1) or low (0). SAFETY: Pi GPIO is 3.3V, max 16mA per pin, 50mA total. Never connect 5V signals. Use resistors for LEDs (330Ω min). Refuses reserved pins.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "gpio":  { "type": "integer", "description": "BCM GPIO number (0-27)" },
                    "value": { "type": "integer", "description": "0 (low) or 1 (high)" }
                },
                "required": ["gpio", "value"]
            }
        },
        {
            "name": "gpio_pulse",
            "description": "Pulse a GPIO pin high for a specified duration then return low. Useful for buzzers, relay triggers, LED blinks. SAFETY: same 3.3V/16mA limits as gpio_write.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "gpio":        { "type": "integer", "description": "BCM GPIO number" },
                    "duration_ms": { "type": "integer", "description": "Pulse duration in milliseconds (default 100)" }
                },
                "required": ["gpio"]
            }
        },
        {
            "name": "gpio_pwm",
            "description": "Set hardware PWM on a PWM-capable GPIO (12, 13, 18, or 19). Requires dtoverlay=pwm-2chan in /boot/firmware/config.txt. Uses sysfs /sys/class/pwm/.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "gpio":      { "type": "integer", "description": "BCM GPIO number — must be 12, 13, 18, or 19" },
                    "duty_pct":  { "type": "number",  "description": "Duty cycle 0.0–100.0 percent" },
                    "freq_hz":   { "type": "number",  "description": "PWM frequency in Hz (default 1000)" }
                },
                "required": ["gpio", "duty_pct"]
            }
        },
        {
            "name": "gpio_servo",
            "description": "Set a servo position by angle. Outputs 50Hz PWM with 1ms–2ms pulse width for 0°–180°. GPIO must be PWM-capable (12, 13, 18, 19). Servo signal is 3.3V-compatible but servo POWER must come from an external 5V supply, not Pi GPIO pins.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "gpio":      { "type": "integer", "description": "BCM GPIO number (12, 13, 18, or 19)" },
                    "angle_deg": { "type": "number",  "description": "Servo angle 0–180 degrees" }
                },
                "required": ["gpio", "angle_deg"]
            }
        },
        {
            "name": "display_face",
            "description": "Set the expression on APEX's on-screen face — your face on this device's display. Emote with it while you talk: it carries tone the text can't. Expressions: neutral, happy, curious, amused, confused, sad, surprised, wink, skeptical, proud, love, focused (the activity states idle/thinking/speaking/listening/alert/sleeping are set for you automatically — you rarely need them). The expression lingers until your next reply, so set it to match your mood. Works on the Slint UI (kiosk/desktop) and the original GC9A01A round-TFT if present; a no-op (not an error) on a headless node.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "state":     { "type": "string", "description": "Expression: neutral|happy|curious|amused|confused|sad|surprised|wink|skeptical|proud|love|focused (or an activity state idle|thinking|speaking|listening|alert|sleeping)" },
                    "gaze":      { "type": "string", "description": "Optional eye direction: center|left|right|up|down (default center)" },
                    "intensity": { "type": "number", "description": "Optional 0.0–1.0, how strong the expression reads (default 0.7)" },
                    "text":      { "type": "string", "description": "Optional short caption shown under the face (≤20 chars)" }
                },
                "required": ["state"]
            }
        },
        {
            "name": "git_status",
            "description": "Show a git working-tree summary (branch + changed/staged/untracked files, short format). `path` is the repo directory (default: your workspace). Confined to the workspace + AGENTD_GIT_ROOTS.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Repo directory (default: workspace)" } }
            }
        },
        {
            "name": "git_diff",
            "description": "Show changes as a stat summary + patch. `staged: true` diffs the index instead of the working tree. Output is length-capped.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":   { "type": "string", "description": "Repo directory (default: workspace)" },
                    "staged": { "type": "boolean", "description": "Diff staged (index) changes instead of working tree (default false)" }
                }
            }
        },
        {
            "name": "git_log",
            "description": "Recent commits, one line each (with refs).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repo directory (default: workspace)" },
                    "n":    { "type": "integer", "description": "How many commits (default 10, max 200)" }
                }
            }
        },
        {
            "name": "git_branch",
            "description": "With `name`: create a branch. Without: list all branches.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repo directory (default: workspace)" },
                    "name": { "type": "string", "description": "New branch name (omit to list)" }
                }
            }
        },
        {
            "name": "git_init",
            "description": "Initialize a git repository in `path` (default: your workspace) so it can be version-controlled.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Directory to init (default: workspace)" } }
            }
        },
        {
            "name": "git_commit",
            "description": "Stage and commit. `paths` stages just those files; omit to stage all changes. Committer is your agent identity. (Allowed inside the workspace; for a repo outside it — added via AGENTD_GIT_ROOTS — the edits themselves still go through gated tools.)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":    { "type": "string", "description": "Repo directory (default: workspace)" },
                    "message": { "type": "string", "description": "Commit message" },
                    "paths":   { "type": "array", "items": { "type": "string" }, "description": "Files to stage (default: all changes)" }
                },
                "required": ["message"]
            }
        },
        {
            "name": "git_push",
            "description": "Push to a remote (default origin). Publishes externally — approval-gated.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":   { "type": "string", "description": "Repo directory (default: workspace)" },
                    "remote": { "type": "string", "description": "Remote name (default origin)" },
                    "branch": { "type": "string", "description": "Branch to push (default: current)" }
                }
            }
        },
        {
            "name": "git_checkout",
            "description": "Switch to a branch / commit / restore a path. Rewrites the working tree — approval-gated.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repo directory (default: workspace)" },
                    "ref":  { "type": "string", "description": "Branch, commit, or path to check out" }
                },
                "required": ["ref"]
            }
        },
        {
            "name": "git_reset",
            "description": "Reset HEAD to `ref` (default HEAD). `hard: true` also discards working-tree changes (destructive) — approval-gated.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repo directory (default: workspace)" },
                    "ref":  { "type": "string", "description": "Target ref (default HEAD)" },
                    "hard": { "type": "boolean", "description": "Discard working-tree changes (default false)" }
                }
            }
        },
        {
            "name": "git_merge",
            "description": "Merge `branch` into the current branch (no-edit). May rewrite the tree / conflict — approval-gated.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":   { "type": "string", "description": "Repo directory (default: workspace)" },
                    "branch": { "type": "string", "description": "Branch to merge in" }
                },
                "required": ["branch"]
            }
        },
        {
            "name": "eject_media",
            "description": "Safely eject (unmount) a removable exo-workspace USB stick by its label, so the user can physically pull it out without corrupting it. Offer this when a task that used the stick is finished and the user is done with it — e.g. 'want me to eject it now that I've saved the report?'. The label is the APEX-… folder name under media/ (it's listed in your embodiment block, or run list_dir(\"media\")). Flushes pending writes and unmounts; the stick stays safe to re-plug. Only acts on APEX-* exo-workspace sticks — never a system disk.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "The stick's exo-workspace label, e.g. 'APEX-config' — the folder name under media/" }
                },
                "required": ["label"]
            }
        }
    ])
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

pub fn call(name: &str, args: &Value) -> Value {
    // Pin the supervisor-stamped per-agent workspace for this dispatch (cleared
    // on drop). Absent (direct MCP / tests) → workspace_base() falls back to the
    // process-global AGENTD_WORKSPACE. The model can't widen this: the supervisor
    // overwrites any model-supplied `__workspace` before the call reaches here.
    let _ws = WorkspaceGuard::enter(
        args.get("__workspace").and_then(Value::as_str).map(str::to_owned),
    );
    match name {
        "run_command" => run_command(args),
        "read_file" => read_file(args),
        "write_file" => write_file(args),
        "list_dir" => list_dir(args),
        "notes_list" => notes_list(),
        "notes_read" => notes_read(args),
        "notes_append" => notes_append(args),
        "sketch_snapshot" => sketch_snapshot(),
        "sketch_draw" => sketch_draw(args),
        "screenshot_mirror" => screenshot_mirror(),
        "camera_capture" => camera_capture(args),
        "eject_media" => eject_media(args),
        "create_dir" => create_dir(args),
        "delete_path" => delete_path(args),
        "http_fetch" => http_fetch(args),
        "cpu_temp" => cpu_temp(),
        "disk_usage" => disk_usage(args),
        "memory_info" => memory_info(),
        "uptime" => uptime(),
        "notify" => notify(args),
        "audio_analyze" => audio_analyze(args),
        "audio_trim_silence" => audio_trim_silence(args),
        "audio_normalize" => audio_normalize(args),
        "audio_peak_limit" => audio_peak_limit(args),
        "audio_trim" => audio_trim(args),
        "audio_clean" => audio_clean(args),
        "gpio_info" => gpio_info(),
        "gpio_read" => gpio_read(args),
        "gpio_write" => gpio_write(args),
        "gpio_pulse" => gpio_pulse(args),
        "gpio_pwm" => gpio_pwm(args),
        "gpio_servo" => gpio_servo(args),
        "display_face" => display_face(args),
        "git_status" => git_status(args),
        "git_diff" => git_diff(args),
        "git_log" => git_log(args),
        "git_branch" => git_branch(args),
        "git_init" => git_init(args),
        "git_commit" => git_commit(args),
        "git_push" => git_push(args),
        "git_checkout" => git_checkout(args),
        "git_reset" => git_reset(args),
        "git_merge" => git_merge(args),
        _ => tool_error(format!("unknown tool: {}", name)),
    }
}

fn tool_ok(content: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": content.to_string() }] })
}

fn tool_error(msg: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": json!({"error": msg.into()}).to_string() }], "isError": true })
}

// ─── Denylist ────────────────────────────────────────────────────────────────

fn denylist_check(cmd: &str) -> Option<&'static str> {
    let trimmed = cmd.trim();

    // Disk destruction
    if trimmed.starts_with("mkfs") {
        return Some("mkfs commands are blocked");
    }
    if trimmed.contains("wipefs") {
        return Some("wipefs is blocked");
    }

    // Raw device writes via dd
    if trimmed.starts_with("dd") {
        let lower = trimmed.to_lowercase();
        if lower.contains("of=/dev/sd")
            || lower.contains("of=/dev/nvme")
            || lower.contains("of=/dev/mmcblk")
        {
            return Some("dd to raw block devices is blocked");
        }
    }

    // Partition table editors on real devices
    for tool in &["fdisk", "parted", "gdisk"] {
        if trimmed.starts_with(tool) && trimmed.contains("/dev/") {
            return Some("partition table editing on real devices is blocked");
        }
    }

    // rm -rf / (and variants)
    if trimmed.contains("rm") && trimmed.contains("-r") {
        let lower = trimmed.to_lowercase();
        // Match rm -rf / or rm -rf --no-preserve-root /
        if lower.contains("--no-preserve-root")
            || (trimmed.ends_with(" /") || trimmed.contains(" / "))
        {
            // Check it's actually targeting root
            if lower.contains(" /")
                && !lower.contains("/var")
                && !lower.contains("/tmp")
                && !lower.contains("/home")
                && !lower.contains("/opt")
            {
                return Some("rm -rf / is blocked");
            }
        }
    }

    // System directory destruction
    for protected in &[
        "rm -rf /usr",
        "rm -rf /bin",
        "rm -rf /lib",
        "rm -rf /sbin",
        "rm -rf /boot",
        "rm -rf /etc/passwd",
        "rm -rf /etc/shadow",
    ] {
        if trimmed.contains(protected) {
            return Some("destruction of system directories is blocked");
        }
    }

    // Truncation of critical auth files
    if (trimmed.starts_with("> /etc/passwd") || trimmed.starts_with("> /etc/shadow"))
        || trimmed.contains("truncate") && trimmed.contains("/etc/passwd")
        || trimmed.contains("truncate") && trimmed.contains("/etc/shadow")
    {
        return Some("truncating auth files is blocked");
    }

    // Fork bomb pattern
    if trimmed.contains(":(){ :|:") {
        return Some("fork bomb pattern is blocked");
    }

    None
}

// ─── Tool implementations ────────────────────────────────────────────────────

fn run_command(args: &Value) -> Value {
    let cmd = match args["cmd"].as_str() {
        Some(c) => c,
        None => return tool_error("cmd is required"),
    };

    if let Some(reason) = denylist_check(cmd) {
        return tool_error(format!("BLOCKED: {}", reason));
    }

    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30).min(300);
    let cwd = args["cwd"].as_str();

    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(cmd);

    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    if let Some(env_map) = args["env"].as_object() {
        for (k, v) in env_map {
            if let Some(val) = v.as_str() {
                command.env(k, val);
            }
        }
    }

    use std::sync::mpsc;
    use std::thread;

    let child = match command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return tool_error(format!("failed to spawn: {}", e)),
    };

    let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(Ok(output)) => tool_ok(json!({
            "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
            "exit_code": output.status.code().unwrap_or(-1),
            "timed_out": false
        })),
        Ok(Err(e)) => tool_error(format!("command error: {}", e)),
        Err(_) => tool_ok(json!({
            "stdout": "",
            "stderr": format!("command timed out after {}s", timeout_secs),
            "exit_code": -1,
            "timed_out": true
        })),
    }
}

// ─── Per-agent workspace (the supervisor-stamped `__workspace`) ─────────────────
// apexos-tools is ONE process shared by every agent, so the per-agent workspace
// root can't live in a process-global env var — it arrives per tool call as the
// supervisor-stamped `__workspace` arg (system-set, like `agent_id` for Cerebro:
// the stamp overwrites any model-supplied value, so a model can't redirect its
// own confinement). `call()` pins it into this thread-local for the duration of
// the dispatch. The MCP server is single-threaded + synchronous (one `call()` at
// a time — see main.rs), and the RAII guard clears it on drop, so a reused thread
// can never leak a stale root. Absent (direct MCP caller / tests) → fall back to
// the process-global `AGENTD_WORKSPACE`, byte-identical to the pre-per-agent path.
thread_local! {
    static CALL_WORKSPACE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// RAII guard: pins the per-call workspace, clears it on drop.
struct WorkspaceGuard;
impl WorkspaceGuard {
    fn enter(ws: Option<String>) -> Self {
        CALL_WORKSPACE.with(|c| *c.borrow_mut() = ws.filter(|s| !s.is_empty()));
        WorkspaceGuard
    }
}
impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        CALL_WORKSPACE.with(|c| *c.borrow_mut() = None);
    }
}

/// The workspace base for the current call: the supervisor-stamped per-agent root
/// if present, else the process-global `AGENTD_WORKSPACE`, else the default. APEX
/// and unbound sessions stamp the node base here, so the no-binding path stays
/// byte-identical to the pre-per-agent single workspace.
fn workspace_base() -> String {
    CALL_WORKSPACE
        .with(|c| c.borrow().clone())
        .or_else(|| std::env::var("AGENTD_WORKSPACE").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string())
}

/// Root a relative path onto the agent workspace; absolute paths pass through
/// unchanged. Relative paths join [`workspace_base`] (the per-agent root, default
/// `/var/lib/agentd/workspace`) so e.g. `read_file("notes.txt")` resolves there
/// instead of against the process CWD (which is `/` under systemd).
fn resolve_path(path: &str) -> std::path::PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    Path::new(&workspace_base()).join(p)
}

// ─── Filesystem confinement ────────────────────────────────────────────────────
// Single source of truth for which paths the file tools may touch. The tool
// process is the ONLY enforcement gate for the read tools (read_file/list_dir
// are policy `allow`, so the user is never prompted); confinement therefore has
// to live here, not just in the approval layer. Model (see CLAUDE.md):
//   • writes/creates/deletes → workspace only, hard.
//   • reads/lists           → workspace + a small read allowlist, minus an
//                             always-blocked secret denylist.

/// The agent workspace root for the current call, canonicalized (per-agent via
/// [`workspace_base`]). Defaults to `/var/lib/agentd/workspace`. Falls back to the
/// literal path when it cannot be canonicalized (e.g. on a dev box where the dir
/// does not exist).
fn workspace_root() -> PathBuf {
    let ws = workspace_base();
    // Ensure the per-agent root exists before canonicalizing: (a) a missing dir
    // would canonicalize-fail and fall back to the literal path, which can
    // mismatch the canonicalized target and spuriously reject a legitimate write;
    // (b) it lets the first write into a freshly-bound agent's workspace succeed
    // even if provisioning at agent-create was skipped. Idempotent + cheap.
    let _ = fs::create_dir_all(&ws);
    fs::canonicalize(&ws).unwrap_or_else(|_| PathBuf::from(&ws))
}

/// Extra roots **reads** (never writes) may reach beyond the workspace.
/// Default-deny everywhere else. The operator can extend this without a
/// recompile via `AGENTD_READ_ROOTS` (colon-separated absolute paths).
fn read_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/etc/agentd/parts"), // EDK on-hand parts inventory (read-only)
        PathBuf::from("/sys"),              // thermal / board diagnostics
        PathBuf::from("/proc/cpuinfo"),     // board model / core count
        PathBuf::from("/proc/meminfo"),     // memory diagnostics
        PathBuf::from("/var/lib/agentd/update"), // self-update outcome markers (confirmed/rolled-back.json)
    ];
    if let Ok(extra) = std::env::var("AGENTD_READ_ROOTS") {
        roots.extend(extra.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    roots
}

/// Paths that are NEVER readable, even inside an allowed read root — defence in
/// depth against secret exfiltration if an operator widens `AGENTD_READ_ROOTS`.
fn is_secret_path(canon: &Path) -> bool {
    let s = canon.to_string_lossy();
    s.starts_with("/etc/agentd/env")                       // AGENTD_TOKEN + provider keys
        || s == "/etc/shadow" || s == "/etc/gshadow"
        || s.contains("/.ssh/") || s.ends_with("/.ssh")
        || s.ends_with(".api_key") || s.ends_with(".oai_api_key")
        || (s.starts_with("/proc/") && s.ends_with("/environ")) // /proc/<pid>/environ
}

/// Resolve and confine a filesystem tool path. Relative paths root onto the
/// workspace. `write = true` (write/create/delete) confines to the workspace;
/// `write = false` (read/list) also accepts the read allowlist minus the secret
/// denylist. Returns the canonical path to operate on, or an error string to
/// hand back to the agent. This is the single confinement gate for FS tools.
///
/// The mechanism (reject `..`, lenient-canonicalize, root containment) lives in the
/// std-only `apexos-confine` crate; here we map our policy — the per-agent workspace,
/// the read allowlist, the secret denylist — into it and render the agent-facing
/// error strings.
fn confine(path: &str, write: bool) -> Result<PathBuf, String> {
    let resolved = resolve_path(path);
    let access = if write { Access::Write } else { Access::Read };
    apexos_confine::confine_fs(&resolved, access, &workspace_root(), &read_roots(), is_secret_path)
        .map_err(|d| match d {
            Denied::Traversal => "path traversal (..) is not allowed".to_string(),
            Denied::Unresolvable(p) => format!("cannot resolve path {}", p.display()),
            Denied::OutsideWorkspace { workspace, path } => format!(
                "write/delete is confined to the workspace ({}); {} is outside it",
                workspace.display(),
                path.display(),
            ),
            Denied::Secret(p) => format!("reading {} is blocked (sensitive)", p.display()),
            Denied::OutsideReadAllowlist(p) => format!(
                "reading {} is outside the workspace and the read allowlist",
                p.display(),
            ),
            Denied::OutsideRoots(p) => format!("{} is outside the allowed roots", p.display()),
        })
}

// ─── Git tools ────────────────────────────────────────────────────────────────
// git_* shell out to the system `git` via cmd_capture — argv, never `/bin/sh`, so
// there is no shell-injection surface; a leading-`-` guard defeats option
// injection on positional refs/branches/paths. Repos are confined to a "git
// roots" allowlist (mirrors read_roots): the agent workspace by default, extended
// by AGENTD_GIT_ROOTS (colon-sep absolute paths, e.g. the agentd source repo for
// the self-update loop). Policy: read-only verbs + git_init/branch/commit are
// `allow` (so they MUST self-confine, like read_file — git_roots is the gate);
// push/checkout/reset/merge (publish or rewrite the tree) are `ask`. With the
// default (no AGENTD_GIT_ROOTS) the only git root is the workspace, so an `allow`
// commit can only ever touch the agent's own workspace repo.

/// Directories a git op may touch: the workspace + AGENTD_GIT_ROOTS (colon-sep).
fn git_roots() -> Vec<PathBuf> {
    let mut roots = vec![workspace_root()];
    if let Ok(extra) = std::env::var("AGENTD_GIT_ROOTS") {
        roots.extend(
            extra
                .split(':')
                .filter(|s| !s.is_empty())
                .map(|s| fs::canonicalize(s).unwrap_or_else(|_| PathBuf::from(s))),
        );
    }
    roots
}

/// Resolve + confine a repo path (relative → workspace; default `.` = workspace)
/// to within a git root. Rejects `..`; canonicalizes. Returns the dir for `git -C`.
fn confine_git_repo(path: Option<&str>) -> Result<PathBuf, String> {
    let resolved = resolve_path(path.unwrap_or("."));
    apexos_confine::confine_to_roots(&resolved, &git_roots()).map_err(|d| match d {
        Denied::Traversal => "path traversal (..) is not allowed".to_string(),
        Denied::Unresolvable(p) => format!("cannot resolve path {}", p.display()),
        Denied::OutsideRoots(p) => {
            format!("{} is outside the git roots (workspace + AGENTD_GIT_ROOTS)", p.display())
        }
        _ => unreachable!("confine_to_roots only yields Traversal/Unresolvable/OutsideRoots"),
    })
}

/// Reject a positional arg git could read as an option (leading `-`).
fn git_safe_arg(label: &str, v: &str) -> Result<(), String> {
    if v.starts_with('-') {
        return Err(format!("{label} may not start with '-': {v:?}"));
    }
    Ok(())
}

/// Run `git -C <repo> <args…>` (args exclude the leading `-C <repo>`) and shape
/// the result. Output is char-capped so a huge diff/log can't blow the context.
fn run_git(repo: &Path, args: Vec<String>) -> Value {
    let repo_s = repo.to_string_lossy().into_owned();
    let mut full: Vec<String> = vec!["-C".into(), repo_s.clone()];
    full.extend(args);
    let refs: Vec<&str> = full.iter().map(String::as_str).collect();
    let (out, err, ok) = cmd_capture("git", &refs);
    if !ok {
        return tool_error(format!("git failed: {}", err.trim()));
    }
    let out = out.trim_end();
    let capped: String = if out.chars().count() > 16000 {
        out.chars().take(16000).collect::<String>() + "\n… (truncated)"
    } else {
        out.to_string()
    };
    tool_ok(json!({ "repo": repo_s, "output": capped }))
}

/// Committer identity = the node agent id (AGENTD_AGENT_ID, default APEX), so
/// commits are attributable without per-repo `git config user.*`.
fn git_identity() -> (String, String) {
    let id = std::env::var("AGENTD_AGENT_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "APEX".to_string());
    let email = format!("{}@apexos.local", id.to_lowercase());
    (id, email)
}

/// Shared repo-arg resolution for the git tools.
fn git_repo_arg(args: &Value) -> Result<PathBuf, Value> {
    confine_git_repo(args["path"].as_str()).map_err(tool_error)
}

fn git_status(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    run_git(&repo, vec!["status".into(), "--short".into(), "--branch".into()])
}

fn git_diff(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    let mut a = vec!["diff".into(), "--stat".into(), "--patch".into()];
    if args["staged"].as_bool().unwrap_or(false) {
        a.push("--staged".into());
    }
    run_git(&repo, a)
}

fn git_log(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    let n = args["n"].as_u64().unwrap_or(10).clamp(1, 200).to_string();
    run_git(&repo, vec!["log".into(), "--oneline".into(), "--decorate".into(), "-n".into(), n])
}

fn git_branch(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    match args["name"].as_str().filter(|s| !s.is_empty()) {
        Some(name) => {
            if let Err(e) = git_safe_arg("branch name", name) { return tool_error(e); }
            run_git(&repo, vec!["branch".into(), name.to_string()])
        }
        None => run_git(&repo, vec!["branch".into(), "--all".into()]),
    }
}

fn git_init(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    run_git(&repo, vec!["init".into()])
}

fn git_commit(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    let message = match args["message"].as_str() {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => return tool_error("message is required"),
    };
    let paths: Vec<String> = args["paths"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    for p in &paths {
        if let Err(e) = git_safe_arg("path", p) { return tool_error(e); }
    }
    // Stage: explicit paths (after `--`), else everything.
    let mut add: Vec<String> = vec!["add".into()];
    if paths.is_empty() {
        add.push("-A".into());
    } else {
        add.push("--".into());
        add.extend(paths);
    }
    {
        let repo_s = repo.to_string_lossy().into_owned();
        let mut full: Vec<String> = vec!["-C".into(), repo_s];
        full.extend(add);
        let refs: Vec<&str> = full.iter().map(String::as_str).collect();
        let (_, err, ok) = cmd_capture("git", &refs);
        if !ok {
            return tool_error(format!("git add: {}", err.trim()));
        }
    }
    let (name, email) = git_identity();
    run_git(
        &repo,
        vec![
            "-c".into(), format!("user.name={name}"),
            "-c".into(), format!("user.email={email}"),
            "commit".into(), "-m".into(), message,
        ],
    )
}

fn git_push(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    let remote = args["remote"].as_str().unwrap_or("origin").to_string();
    if let Err(e) = git_safe_arg("remote", &remote) { return tool_error(e); }
    let mut a = vec!["push".into(), remote];
    if let Some(b) = args["branch"].as_str().filter(|s| !s.is_empty()) {
        if let Err(e) = git_safe_arg("branch", b) { return tool_error(e); }
        a.push(b.to_string());
    }
    run_git(&repo, a)
}

fn git_checkout(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    let r = match args["ref"].as_str().filter(|s| !s.is_empty()) {
        Some(r) => r,
        None => return tool_error("ref is required"),
    };
    if let Err(e) = git_safe_arg("ref", r) { return tool_error(e); }
    run_git(&repo, vec!["checkout".into(), r.to_string()])
}

fn git_reset(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    let r = args["ref"].as_str().filter(|s| !s.is_empty()).unwrap_or("HEAD");
    if let Err(e) = git_safe_arg("ref", r) { return tool_error(e); }
    let mut a = vec!["reset".into()];
    if args["hard"].as_bool().unwrap_or(false) {
        a.push("--hard".into());
    }
    a.push(r.to_string());
    run_git(&repo, a)
}

fn git_merge(args: &Value) -> Value {
    let repo = match git_repo_arg(args) { Ok(r) => r, Err(e) => return e };
    let b = match args["branch"].as_str().filter(|s| !s.is_empty()) {
        Some(b) => b,
        None => return tool_error("branch is required"),
    };
    if let Err(e) = git_safe_arg("branch", b) { return tool_error(e); }
    run_git(&repo, vec!["merge".into(), "--no-edit".into(), b.to_string()])
}

fn read_file(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return tool_error("path is required"),
    };
    let path = match confine(path, false) {
        Ok(p) => p,
        Err(e) => return tool_error(e),
    };
    let max_bytes = args["max_bytes"].as_u64().unwrap_or(1_048_576) as usize;

    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => return tool_error(format!("cannot open {}: {}", path.display(), e)),
    };

    let size = file.metadata().map(|m| m.len()).unwrap_or(0);

    // Read up to max_bytes robustly. metadata().len() is 0 for /proc and /sys
    // files and may lag for growing files, so we cannot size the buffer from it.
    // Take one extra byte to detect whether the file continued past max_bytes.
    let mut buf = Vec::new();
    if let Err(e) = file.take(max_bytes as u64 + 1).read_to_end(&mut buf) {
        return tool_error(format!("read error: {}", e));
    }
    let truncated = buf.len() > max_bytes;
    buf.truncate(max_bytes);

    let content = String::from_utf8_lossy(&buf).to_string();

    tool_ok(json!({
        "content": content,
        "size_bytes": size,
        "truncated": truncated
    }))
}

fn write_file(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return tool_error("path is required"),
    };
    let content = match args["content"].as_str() {
        Some(c) => c,
        None => return tool_error("content is required"),
    };
    let append = args["append"].as_bool().unwrap_or(false);
    let path = match confine(path, true) {
        Ok(p) => p,
        Err(e) => return tool_error(e),
    };

    // Create parent dirs if needed
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    use std::io::Write as IoWrite;
    use std::fs::OpenOptions;

    let mut file = match OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => return tool_error(format!("cannot open {}: {}", path.display(), e)),
    };

    match file.write_all(content.as_bytes()) {
        Ok(_) => tool_ok(json!({ "bytes_written": content.len() })),
        Err(e) => tool_error(format!("write error: {}", e)),
    }
}

// ─── Notes ───────────────────────────────────────────────────────────────────
// The shared notebook: plain markdown files under <workspace>/notes, the same
// dir the gateway's /api/notes routes (and the Notes UI app) read and write.
// notes_append lets APEX leave the user a note without knowing the path.

/// The notes directory: <AGENTD_WORKSPACE or /var/lib/agentd/workspace>/notes.
fn notes_dir() -> std::path::PathBuf {
    resolve_path("notes")
}

/// Reduce an arbitrary name to a safe `.md` filename: strip path components
/// (defeats `../`), force a `.md` extension. None if nothing usable remains.
fn sanitize_note_name(name: &str) -> Option<String> {
    let stem = Path::new(name.trim()).file_name().and_then(|s| s.to_str())?.trim();
    if stem.is_empty() || stem == "." || stem == ".." {
        return None;
    }
    let stem = stem.strip_suffix(".md").unwrap_or(stem);
    if stem.is_empty() { return None; }
    Some(format!("{stem}.md"))
}

fn notes_list() -> Value {
    let dir = notes_dir();
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(ext, "md" | "markdown" | "txt") { continue; }
            if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                names.push(n.to_string());
            }
        }
    }
    names.sort();
    tool_ok(json!({ "notes": names }))
}

fn notes_read(args: &Value) -> Value {
    let name = match args["name"].as_str().and_then(sanitize_note_name) {
        Some(n) => n,
        None => return tool_error("a valid note name is required"),
    };
    let path = notes_dir().join(&name);
    match fs::read_to_string(&path) {
        Ok(content) => tool_ok(json!({ "name": name, "content": content })),
        Err(e) => tool_error(format!("cannot read {}: {}", name, e)),
    }
}

fn notes_append(args: &Value) -> Value {
    let name = match args["name"].as_str().and_then(sanitize_note_name) {
        Some(n) => n,
        None => return tool_error("a valid note name is required"),
    };
    let text = match args["text"].as_str() {
        Some(t) => t,
        None => return tool_error("text is required"),
    };
    let dir = notes_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        return tool_error(format!("cannot create notes dir: {}", e));
    }
    let path = dir.join(&name);

    use std::io::Write as IoWrite;
    use std::fs::OpenOptions;
    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => return tool_error(format!("cannot open {}: {}", name, e)),
    };
    let line = if text.ends_with('\n') { text.to_string() } else { format!("{text}\n") };
    match file.write_all(line.as_bytes()) {
        Ok(_) => tool_ok(json!({ "name": name, "appended_bytes": line.len() })),
        Err(e) => tool_error(format!("write error: {}", e)),
    }
}

fn sketch_snapshot() -> Value {
    // The Sketchpad app saves the current canvas to <workspace>/sketches/latest.png
    // via the gateway. Return the vision sentinel so agentd's turn loop shims the PNG
    // and hands APEX the image inline — it sees the drawing directly (see
    // apexos_core::vision and agent/src/turn.rs::vision_rewrite).
    let path = resolve_path("sketches/latest.png");
    if path.exists() {
        tool_ok(json!({
            "vision": { "path": path.to_string_lossy() },
            "text": "Here is the current sketch from the Sketchpad canvas.",
        }))
    } else {
        tool_ok(json!({ "path": null, "message": "No sketch yet — the user hasn't sent one from the Sketchpad." }))
    }
}

/// Screen mirror (#36): capture APEX's own live UI and hand it back via the
/// vision sentinel. The Slint UI serves a PNG of itself at a loopback endpoint
/// (renderer-agnostic Window::take_snapshot — no DRM/Wayland capture needed); we
/// fetch it, persist under the workspace, and return {vision:{path}} so the turn
/// loop downscales it and APEX sees its screen inline. Path form (not b64) keeps
/// the tool-result JSON small for a full-screen image.
fn screenshot_mirror() -> Value {
    let url = std::env::var("APEXOS_UI_SNAPSHOT_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8788/snapshot".to_string());

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return tool_error(format!("client build failed: {}", e)),
    };

    let resp = match client.get(&url).send() {
        Ok(r) => r,
        // Connection refused / unreachable = no UI on this node (headless, or the
        // kiosk UI is down). Not an error — just nothing to mirror.
        Err(_) => {
            return tool_ok(json!({
                "screen": null,
                "message": "No display attached — the ApexOS-RS UI isn't running on this node (headless, or the kiosk display is down). Nothing to mirror."
            }))
        }
    };
    if !resp.status().is_success() {
        return tool_error(format!("snapshot endpoint returned HTTP {}", resp.status().as_u16()));
    }
    let bytes = match resp.bytes() {
        Ok(b) => b,
        Err(e) => return tool_error(format!("reading snapshot failed: {}", e)),
    };

    let path = resolve_path("screenshots/latest.png");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(&path, &bytes) {
        return tool_error(format!("cannot write screenshot {}: {}", path.display(), e));
    }
    tool_ok(json!({
        "vision": { "path": path.to_string_lossy() },
        "text": "This is APEX's current screen — the live ApexOS-RS UI as rendered right now.",
    }))
}

/// Camera eyes: snap one frame from whatever camera this device has and hand it
/// back via the vision sentinel — APEX sees the physical world inline, the same
/// path as screenshot_mirror / sketch_snapshot. Renderer of capture is chosen at
/// runtime (the capture half of HW-tier detection): Pi CSI camera (rpicam /
/// libcamera) first, then a USB / laptop webcam over V4L2 (ffmpeg), then fswebcam.
/// Self-contained — no agentd coupling. Graceful "no camera" like screenshot_mirror's
/// "no display": a note, not an error.
fn camera_capture(args: &Value) -> Value {
    let out = resolve_path("camera/latest.jpg");
    if let Some(parent) = out.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let out_s = out.to_string_lossy().to_string();
    let _ = fs::remove_file(&out); // ensure a stale frame can't masquerade as fresh

    // 1) Operator override — a full custom capture command with a {out} placeholder.
    //    Lets any exotic backend (gphoto2, a network cam grab, …) feed the same flow.
    if let Some(cmd) = std::env::var("APEXOS_CAMERA_CMD").ok().filter(|s| !s.is_empty()) {
        let cmd = cmd.replace("{out}", &out_s);
        let (_o, err, ok) = cmd_capture("/bin/sh", &["-c", &cmd]);
        if ok && jpeg_ok(&out) {
            return camera_ok(&out, "custom", None);
        }
        return tool_error(format!("APEXOS_CAMERA_CMD failed: {}", err.trim()));
    }

    // 2) Explicit device → V4L2 (USB / laptop webcam) via ffmpeg.
    let forced = args["device"].as_str()
        .map(|s| s.to_string())
        .or_else(|| std::env::var("APEXOS_CAMERA_DEVICE").ok().filter(|s| !s.is_empty()));
    if let Some(dev) = forced {
        if try_v4l2_ffmpeg(&dev, &out) && jpeg_ok(&out) {
            return camera_ok(&out, "v4l2-ffmpeg", Some(&dev));
        }
        if try_fswebcam(&dev, &out) && jpeg_ok(&out) {
            return camera_ok(&out, "fswebcam", Some(&dev));
        }
        return tool_error(format!("camera device {} produced no frame (is it a capture node, and is the agentd user in the 'video' group?)", dev));
    }

    // 3) Auto-detect. On a Pi, try the CSI camera first (rpicam/libcamera).
    if gpio_detect_model().to_lowercase().contains("raspberry") {
        for prog in ["rpicam-jpeg", "libcamera-jpeg"] {
            if try_rpicam(prog, &out) && jpeg_ok(&out) {
                return camera_ok(&out, prog, Some("csi:0"));
            }
        }
    }
    // Then any USB / webcam V4L2 nodes, in order.
    for dev in list_video_nodes() {
        if try_v4l2_ffmpeg(&dev, &out) && jpeg_ok(&out) {
            return camera_ok(&out, "v4l2-ffmpeg", Some(&dev));
        }
        if try_fswebcam(&dev, &out) && jpeg_ok(&out) {
            return camera_ok(&out, "fswebcam", Some(&dev));
        }
    }

    // Nothing captured — not an error, just no eyes on this node.
    tool_ok(json!({
        "camera": null,
        "message": "No camera detected on this device — no Pi CSI camera (rpicam) and no working /dev/video* webcam. Attach a camera, or set APEXOS_CAMERA_DEVICE / APEXOS_CAMERA_CMD."
    }))
}

/// Safely eject a mounted exo-workspace stick by its `APEX-<label>`. Shells the
/// root umount helper via the narrow sudoers drop-in (the SAME path the UI ⏏
/// affordance uses); the helper hard-confines the mountpoint to `<workspace>/media/`
/// and re-validates the label. We validate here too — a clear error for the agent,
/// and defence in depth. Non-destructive: it just flushes + unmounts.
fn eject_media(args: &Value) -> Value {
    let label = args["label"].as_str().unwrap_or("").trim().to_string();
    if !valid_exo_label(&label) {
        return tool_error(
            "label must be an exo-workspace stick (APEX-<name>): letters, digits, . _ - . \
             See the mounted sticks in your embodiment block, or run list_dir(\"media\").",
        );
    }
    let (_out, err, ok) = cmd_capture("sudo", &["-n", "/usr/local/lib/apexos/usb-umount", "--label", &label]);
    if ok {
        tool_ok(json!({
            "ejected": label,
            "message": format!("Ejected {label} — safe to physically remove the stick."),
        }))
    } else {
        let detail = err.lines().last().unwrap_or("eject failed").trim();
        tool_error(format!("eject {label} failed: {detail}"))
    }
}

/// A valid exo-workspace filesystem label (mirrors the gateway's `valid_exo_label`):
/// `APEX-` prefix + a sane single component, length-bounded, no traversal. Kept in
/// sync with `apexos_gateway::valid_exo_label` + the `usb-umount` helper's own check.
fn valid_exo_label(label: &str) -> bool {
    label.starts_with("APEX-")
        && (6..=64).contains(&label.len())   // at least one char after "APEX-"
        && !label.contains("..")
        && label.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn camera_ok(path: &Path, backend: &str, device: Option<&str>) -> Value {
    tool_ok(json!({
        "vision": { "path": path.to_string_lossy() },
        "text": "Live frame from this device's camera — what APEX sees through its eye right now.",
        "backend": backend,
        "device": device,
    }))
}

/// A captured frame must exist and be non-trivially sized — guards against a
/// backend that exits 0 but writes an empty/truncated file.
fn jpeg_ok(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.len() > 1024).unwrap_or(false)
}

/// Sorted list of `/dev/video*` capture nodes (video0, video1, …). A USB cam often
/// exposes several nodes (the extras are metadata-only and simply fail to capture,
/// so we just try them in order).
fn list_video_nodes() -> Vec<String> {
    let mut nodes: Vec<(u32, String)> = fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let num = name.strip_prefix("video")?;
            let n: u32 = num.parse().ok()?;
            Some((n, format!("/dev/{}", name)))
        })
        .collect();
    nodes.sort_by_key(|(n, _)| *n);
    nodes.into_iter().map(|(_, p)| p).collect()
}

/// Pi CSI camera via rpicam-jpeg / libcamera-jpeg. `-t 1200` gives ~1.2s of
/// auto-exposure/white-balance warmup before the still — a black frame otherwise.
fn try_rpicam(prog: &str, out: &Path) -> bool {
    let out_s = out.to_string_lossy();
    let (_o, _e, ok) = cmd_capture(prog, &[
        "--nopreview", "--camera", "0", "-t", "1200", "-q", "85",
        "-o", &out_s,
    ]);
    ok
}

/// USB / laptop webcam via ffmpeg's V4L2 input. `-frames:v 5 -update 1` grabs five
/// frames into the same file (last wins) so the sensor has a moment to auto-expose;
/// native resolution (no -video_size) maximizes device compatibility — the vision
/// shim downscales to VISION_MAX_EDGE regardless.
fn try_v4l2_ffmpeg(dev: &str, out: &Path) -> bool {
    let out_s = out.to_string_lossy();
    let (_o, _e, ok) = cmd_capture("ffmpeg", &[
        "-hide_banner", "-loglevel", "error", "-y",
        "-f", "v4l2", "-i", dev,
        "-frames:v", "5", "-update", "1",
        &out_s,
    ]);
    ok
}

/// Last-resort webcam grab via fswebcam (`-S 8` skips warmup frames).
fn try_fswebcam(dev: &str, out: &Path) -> bool {
    let out_s = out.to_string_lossy();
    let (_o, _e, ok) = cmd_capture("fswebcam", &[
        "-d", dev, "-S", "8", "--no-banner", "-q", &out_s,
    ]);
    ok
}

fn list_dir(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return tool_error("path is required"),
    };
    let recursive = args["recursive"].as_bool().unwrap_or(false);
    let path = match confine(path, false) {
        Ok(p) => p,
        Err(e) => return tool_error(e),
    };

    let mut entries = Vec::new();
    collect_dir(&path.to_string_lossy(), recursive, 0, &mut entries);
    tool_ok(json!(entries))
}

fn collect_dir(path: &str, recursive: bool, depth: usize, out: &mut Vec<Value>) {
    if depth > 3 {
        return;
    }
    let read = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in read.flatten() {
        let meta = entry.metadata().ok();
        let kind = meta.as_ref().map(|m| if m.is_dir() { "dir" } else { "file" }).unwrap_or("unknown");
        let size = meta.as_ref().and_then(|m| if m.is_file() { Some(m.len()) } else { None });
        let modified = meta.as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        let mut entry_json = json!({
            "name": entry.path().to_string_lossy(),
            "kind": kind,
        });
        if let Some(s) = size { entry_json["size"] = json!(s); }
        if let Some(m) = modified { entry_json["modified"] = json!(m); }
        out.push(entry_json);

        if recursive && kind == "dir" {
            collect_dir(&entry.path().to_string_lossy(), true, depth + 1, out);
        }
    }
}

fn create_dir(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return tool_error("path is required"),
    };
    let path = match confine(path, true) {
        Ok(p) => p,
        Err(e) => return tool_error(e),
    };
    match fs::create_dir_all(&path) {
        Ok(_) => tool_ok(json!({ "created": path.to_string_lossy() })),
        Err(e) => tool_error(format!("create_dir failed: {}", e)),
    }
}

fn delete_path(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return tool_error("path is required"),
    };

    // Confine to the workspace (rejects `..`, resolves symlinks, blocks escapes)
    // via the shared gate, then operate on the returned *canonical* path — never
    // the raw string — so a symlink swap after the check cannot redirect the
    // delete (closes the TOCTOU the old `metadata(path)`/`remove(path)` had).
    let canonical = match confine(path, true) {
        Ok(p) => p,
        Err(e) => return tool_error(e),
    };

    let recursive = args["recursive"].as_bool().unwrap_or(false);
    let meta = match fs::metadata(&canonical) {
        Ok(m) => m,
        Err(e) => return tool_error(format!("cannot stat {}: {}", canonical.display(), e)),
    };

    let result = if meta.is_dir() {
        if !recursive {
            return tool_error("path is a directory — set recursive=true to delete it");
        }
        fs::remove_dir_all(&canonical)
    } else {
        fs::remove_file(&canonical)
    };

    match result {
        Ok(_) => tool_ok(json!({ "deleted": canonical.to_string_lossy() })),
        Err(e) => tool_error(format!("delete failed: {}", e)),
    }
}

/// Return true if an IP address must not be reachable via http_fetch
/// (SSRF guard): loopback, link-local (incl. cloud metadata 169.254.169.254),
/// and RFC1918 private ranges.
fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()        // 127.0.0.0/8
                || v4.is_link_local() // 169.254.0.0/16
                || v4.is_private()    // 10/8, 172.16/12, 192.168/16
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            // IPv4-mapped (::ffff:a.b.c.d) — check the embedded v4 address.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ip(IpAddr::V4(v4));
            }
            // Link-local fe80::/10 and unique-local fc00::/7.
            let seg = v6.segments()[0];
            (seg & 0xffc0) == 0xfe80 || (seg & 0xfe00) == 0xfc00
        }
    }
}

/// Resolve a URL's host and reject it if any resolved address is in a blocked
/// range. Returns Ok(()) for public hosts. A literal IP host is checked
/// directly.
fn ssrf_guard(url: &str) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {}", e))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?;
    let port = parsed.port_or_known_default().unwrap_or(80);

    // host:port → resolve to one or more socket addresses.
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve host {}: {}", host, e))?;
    let mut any = false;
    for sa in addrs {
        any = true;
        if is_blocked_ip(sa.ip()) {
            return Err(format!(
                "blocked: {} resolves to non-public address {}",
                host,
                sa.ip()
            ));
        }
    }
    if !any {
        return Err(format!("cannot resolve host {}", host));
    }
    Ok(())
}

fn http_fetch(args: &Value) -> Value {
    let url = match args["url"].as_str() {
        Some(u) => u,
        None => return tool_error("url is required"),
    };
    if let Err(e) = ssrf_guard(url) {
        return tool_error(e);
    }
    let method = args["method"].as_str().unwrap_or("GET").to_uppercase();

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        // Re-run the SSRF guard on every redirect hop. The ssrf_guard() above
        // only vets the first URL, so without this a public URL could 302 to a
        // loopback / link-local / RFC1918 address (e.g. 169.254.169.254 cloud
        // metadata). NB: a determined attacker controlling DNS can still rebind
        // between this check and reqwest's own resolve (TOCTOU) — closing that
        // needs a pinned-IP connector and is tracked as a known residual.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.stop()
            } else if ssrf_guard(attempt.url().as_str()).is_err() {
                attempt.error("redirect to non-public address blocked")
            } else {
                attempt.follow()
            }
        }))
        .build()
    {
        Ok(c) => c,
        Err(e) => return tool_error(format!("client build failed: {}", e)),
    };

    let mut req = match method.as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        "HEAD" => client.head(url),
        _ => return tool_error(format!("unsupported method: {}", method)),
    };

    if let Some(headers) = args["headers"].as_object() {
        for (k, v) in headers {
            if let Some(val) = v.as_str() {
                req = req.header(k.as_str(), val);
            }
        }
    }

    if let Some(body) = args["body"].as_str() {
        req = req.body(body.to_string());
    }

    let mut resp = match req.send() {
        Ok(r) => r,
        Err(e) => return tool_error(format!("request failed: {}", e)),
    };

    let status = resp.status().as_u16();
    let resp_headers: serde_json::Map<String, Value> = resp.headers().iter()
        .map(|(k, v)| (k.as_str().to_string(), json!(v.to_str().unwrap_or(""))))
        .collect();

    // Cap response body at 4MB by reading at most the limit (+1 to detect
    // overflow) via a streaming take, rather than buffering the whole body.
    const BODY_LIMIT: usize = 4_194_304;
    let mut body_bytes = Vec::new();
    if let Err(e) = (&mut resp)
        .take(BODY_LIMIT as u64 + 1)
        .read_to_end(&mut body_bytes)
    {
        return tool_error(format!("body read failed: {}", e));
    }
    let body_str = if body_bytes.len() > BODY_LIMIT {
        "[truncated at 4MB]".to_string()
    } else {
        String::from_utf8_lossy(&body_bytes).to_string()
    };

    tool_ok(json!({
        "status": status,
        "body": body_str,
        "headers": resp_headers
    }))
}

fn cpu_temp() -> Value {
    let thermal_base = "/sys/class/thermal";
    let zones = match fs::read_dir(thermal_base) {
        Ok(r) => r,
        Err(e) => return tool_error(format!("cannot read thermal zones: {}", e)),
    };

    let mut readings = Vec::new();
    for entry in zones.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let temp_path = entry.path().join("temp");
        let type_path = entry.path().join("type");

        let raw = match fs::read_to_string(&temp_path) {
            Ok(s) => s.trim().to_string(),
            Err(_) => continue,
        };
        let sensor_type = fs::read_to_string(&type_path)
            .unwrap_or_default()
            .trim()
            .to_string();

        if let Ok(millideg) = raw.parse::<i64>() {
            readings.push(json!({
                "sensor": if sensor_type.is_empty() { name } else { sensor_type },
                "temp_c": millideg as f64 / 1000.0
            }));
        }
    }

    if readings.is_empty() {
        return tool_error("no thermal zones found");
    }

    // Primary is usually the first / highest
    let primary = readings[0].clone();
    tool_ok(json!({
        "temp_c": primary["temp_c"],
        "sensor": primary["sensor"],
        "all_zones": readings
    }))
}

/// Kernel pseudo-filesystems excluded from disk-usage reports. Matched at a path-
/// component boundary so a real mount like `/devel` or `/system` isn't dropped by a
/// bare prefix (the bug this replaces: `"/devel".starts_with("/dev")` was `true`).
fn is_pseudo_mount(m: &str) -> bool {
    m == "none"
        || ["/proc", "/sys", "/dev", "/run"]
            .iter()
            .any(|p| m == *p || m.starts_with(&format!("{p}/")))
}

/// The mountpoint whose filesystem actually holds `fp`: the LONGEST candidate that is
/// `fp` itself or a parent of it at a component boundary. So `/home` beats `/` for
/// `/home/x`, and `/dev` never false-matches `/devel`. (Was a bare `starts_with`,
/// which matched every ancestor mount at once.)
fn best_mount_for<'a>(mounts: &[&'a str], fp: &str) -> Option<&'a str> {
    mounts
        .iter()
        .copied()
        .filter(|m| {
            *m == "/" || fp == *m || fp.starts_with(&format!("{}/", m.trim_end_matches('/')))
        })
        .max_by_key(|m| m.len())
}

fn disk_usage(args: &Value) -> Value {
    let filter_path = args["path"].as_str();

    let mounts_raw = match fs::read_to_string("/proc/mounts") {
        Ok(s) => s,
        Err(e) => return tool_error(format!("cannot read /proc/mounts: {}", e)),
    };

    // Real (non-pseudo) mountpoints, in /proc/mounts order.
    let mounts: Vec<&str> = mounts_raw
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter(|m| !is_pseudo_mount(m))
        .collect();

    let mut results = Vec::new();
    if let Some(fp) = filter_path {
        // Report only the single filesystem that holds fp (the longest match) — not
        // every ancestor mount.
        if let Some(best) = best_mount_for(&mounts, fp) {
            if let Some(stat) = statvfs(best) {
                results.push(stat);
            }
        }
    } else {
        for m in &mounts {
            if let Some(stat) = statvfs(m) {
                results.push(stat);
            }
        }
        if results.is_empty() {
            // Fallback: just do /
            if let Some(stat) = statvfs("/") {
                results.push(stat);
            }
        }
    }

    tool_ok(json!(results))
}

fn statvfs(path: &str) -> Option<Value> {
    // Use `df` command as a portable alternative to calling statvfs syscall directly
    let out = Command::new("df")
        .arg("-B1")
        .arg("--output=source,target,size,used,avail,pcent")
        .arg(path)
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines().skip(1); // skip header
    let line = lines.next()?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }

    let total: u64 = parts[2].parse().unwrap_or(0);
    let used: u64 = parts[3].parse().unwrap_or(0);
    let free: u64 = parts[4].parse().unwrap_or(0);
    let pct = parts[5].trim_end_matches('%').parse::<f64>().unwrap_or(0.0);

    Some(json!({
        "mount": parts[1],
        "total_gb": (total as f64) / 1e9,
        "used_gb": (used as f64) / 1e9,
        "free_gb": (free as f64) / 1e9,
        "pct": pct
    }))
}

fn memory_info() -> Value {
    let raw = match fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(e) => return tool_error(format!("cannot read /proc/meminfo: {}", e)),
    };

    let mut map = std::collections::HashMap::new();
    for line in raw.lines() {
        let mut parts = line.splitn(2, ':');
        if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
            let kb: u64 = val.split_whitespace().next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            map.insert(key.trim().to_string(), kb);
        }
    }

    let total = *map.get("MemTotal").unwrap_or(&0);
    let available = *map.get("MemAvailable").unwrap_or(&0);
    let swap_total = *map.get("SwapTotal").unwrap_or(&0);
    let swap_free = *map.get("SwapFree").unwrap_or(&0);

    tool_ok(json!({
        "total_mb": total / 1024,
        "available_mb": available / 1024,
        "used_mb": (total - available) / 1024,
        "swap_used_mb": (swap_total - swap_free) / 1024
    }))
}

fn uptime() -> Value {
    let raw = match fs::read_to_string("/proc/uptime") {
        Ok(s) => s,
        Err(e) => return tool_error(format!("cannot read /proc/uptime: {}", e)),
    };
    let uptime_secs: f64 = raw.split_whitespace().next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);

    let loadavg = match fs::read_to_string("/proc/loadavg") {
        Ok(s) => s,
        Err(e) => return tool_error(format!("cannot read /proc/loadavg: {}", e)),
    };
    let parts: Vec<&str> = loadavg.split_whitespace().collect();
    let load1: f64 = parts.first().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let load5: f64 = parts.get(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let load15: f64 = parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.0);

    tool_ok(json!({
        "uptime_secs": uptime_secs as u64,
        "load_avg_1": load1,
        "load_avg_5": load5,
        "load_avg_15": load15
    }))
}

fn notify(args: &Value) -> Value {
    let message = match args["message"].as_str() {
        Some(m) => m.to_string(),
        None => return tool_error("message is required"),
    };
    let title = args["title"].as_str().unwrap_or("ApexOS").to_string();
    let tts_skip = args["tts"].as_bool().map(|b| !b).unwrap_or(false);

    let mut fired: Vec<String> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();

    // 1. JSONL log — always, unconditional
    {
        use std::io::Write as IoWrite;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = json!({"ts": ts, "title": title, "message": message});
        match std::fs::OpenOptions::new()
            .create(true).append(true)
            .open("/var/lib/agentd/notifications.jsonl")
        {
            Ok(mut f) => { let _ = writeln!(f, "{}", entry); fired.push("jsonl".into()); }
            Err(e) => { failed.push(json!({"surface": "jsonl", "error": e.to_string()})); }
        }
    }

    // 2. notify-send toast (kiosk display) — fire-and-forget, silently fails if no daemon
    let _ = Command::new("notify-send")
        .arg(&title)
        .arg(&message)
        .spawn();
    fired.push("notify-send".into());

    // 3. TTS — piper if PIPER_MODEL env set, else espeak-ng
    if !tts_skip {
        let tts_ok = if let Ok(model) = std::env::var("PIPER_MODEL") {
            // Pass message via env var to avoid shell quoting issues
            Command::new("/bin/sh")
                .arg("-c")
                .arg("echo \"$_TTS\" | piper --model \"$_MODEL\" --output-raw | aplay -q -r 22050 -f S16_LE -t raw -")
                .env("_TTS", &message)
                .env("_MODEL", &model)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else {
            Command::new("espeak-ng")
                .arg("-s").arg("145")
                .arg(&message)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };

        if tts_ok {
            fired.push("tts".into());
        } else {
            failed.push(json!({"surface": "tts", "error": "espeak-ng/piper unavailable or audio error"}));
        }
    }

    // 4. ntfy.sh — only if NTFY_TOPIC env present
    if let Ok(topic) = std::env::var("NTFY_TOPIC") {
        let ntfy_url = format!("https://ntfy.sh/{}", topic);
        if let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            match client.post(&ntfy_url)
                .header("Title", title.as_str())
                .body(message.clone())
                .send()
            {
                Ok(r) if r.status().is_success() => fired.push("ntfy".into()),
                Ok(r) => failed.push(json!({"surface": "ntfy", "error": format!("HTTP {}", r.status())})),
                Err(e) => failed.push(json!({"surface": "ntfy", "error": e.to_string()})),
            }
        }
    }

    // 5. Telegram — only if BOT_TOKEN + CHAT_ID env present (repo-portable)
    if let (Ok(token), Ok(chat_id)) = (
        std::env::var("TELEGRAM_BOT_TOKEN"),
        std::env::var("TELEGRAM_CHAT_ID"),
    ) {
        let tg_url = format!("https://api.telegram.org/bot{}/sendMessage", token);
        if let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            match client.post(&tg_url)
                .json(&json!({
                    "chat_id": chat_id,
                    "text": format!("*{}*\n{}", title, message),
                    "parse_mode": "Markdown"
                }))
                .send()
            {
                Ok(r) if r.status().is_success() => fired.push("telegram".into()),
                Ok(r) => failed.push(json!({"surface": "telegram", "error": format!("HTTP {}", r.status())})),
                Err(e) => failed.push(json!({"surface": "telegram", "error": e.to_string()})),
            }
        }
    }

    tool_ok(json!({
        "surfaces_fired": fired,
        "surfaces_failed": failed,
    }))
}

// ─── Audio tools ──────────────────────────────────────────────────────────────

/// Run a command and return (stdout, stderr, success).
fn cmd_capture(prog: &str, args: &[&str]) -> (String, String, bool) {
    match Command::new(prog).args(args).output() {
        Ok(o) => (
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
            o.status.success(),
        ),
        Err(e) => (String::new(), e.to_string(), false),
    }
}

/// Extract the last JSON object `{...}` from a string (ffmpeg embeds JSON in log output).
fn extract_json_from_text(text: &str) -> Option<Value> {
    let start = text.rfind('{')?;
    let mut depth = 0usize;
    let mut end = start;
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if depth != 0 { return None; }
    serde_json::from_str(&text[start..end]).ok()
}

/// Core analysis — returns a plain JSON object or an error string.
fn audio_analyze_inner(path: &str) -> Result<Value, String> {
    // 1. ffprobe: streams + format
    let (probe_out, _, probe_ok) = cmd_capture("ffprobe", &[
        "-v", "quiet", "-print_format", "json",
        "-show_streams", "-show_format", path,
    ]);
    if !probe_ok {
        return Err(format!("ffprobe failed on {path}"));
    }
    let probe: Value = serde_json::from_str(&probe_out)
        .map_err(|e| format!("ffprobe parse: {e}"))?;

    let format = probe["format"]["format_name"].as_str().unwrap_or("").split(',').next().unwrap_or("").to_string();
    let duration_s: f64 = probe["format"]["duration"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let bit_rate: u64 = probe["format"]["bit_rate"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);

    let stream0 = &probe["streams"][0];
    let sample_rate: u32 = stream0["sample_rate"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let channels: u32 = stream0["channels"].as_u64().unwrap_or(0) as u32;

    // 2. loudnorm measurement (stderr JSON)
    let (_, ln_stderr, _) = cmd_capture("ffmpeg", &[
        "-i", path,
        "-af", "loudnorm=print_format=json",
        "-f", "null", "-",
    ]);

    let ln = extract_json_from_text(&ln_stderr).unwrap_or_default();
    let lufs_integrated: f64 = ln["input_i"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(-99.0);

    // 3. volumedetect for peak + RMS
    let (_, vd_stderr, _) = cmd_capture("ffmpeg", &[
        "-i", path,
        "-af", "volumedetect",
        "-f", "null", "-",
    ]);
    let peak_db = parse_af_value(&vd_stderr, "max_volume").unwrap_or(-99.0);
    let rms_db  = parse_af_value(&vd_stderr, "mean_volume").unwrap_or(-99.0);

    // 4. silencedetect for tail/head silence
    let (_, sd_stderr, _) = cmd_capture("ffmpeg", &[
        "-i", path,
        "-af", "silencedetect=noise=-50dB:d=0.5",
        "-f", "null", "-",
    ]);
    let (silence_start_s, silence_end_s) = parse_silence(&sd_stderr, duration_s);

    let has_clipping = peak_db > -0.1;
    let dc_offset = ln["input_offset"].as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| v.abs())
        .unwrap_or(0.0);

    Ok(json!({
        "duration_s":       duration_s,
        "sample_rate":      sample_rate,
        "channels":         channels,
        "format":           format,
        "bit_rate":         bit_rate,
        "peak_db":          peak_db,
        "rms_db":           rms_db,
        "lufs_integrated":  lufs_integrated,
        "silence_start_s":  silence_start_s,
        "silence_end_s":    silence_end_s,
        "has_clipping":     has_clipping,
        "dc_offset":        dc_offset,
    }))
}

/// Parse `key: value` float from ffmpeg volumedetect/astats stderr.
fn parse_af_value(text: &str, key: &str) -> Option<f64> {
    for line in text.lines() {
        if line.contains(key) {
            let after_colon = line.split_once(':')?.1;
            let val_str = after_colon.split_whitespace().next()?;
            return val_str.parse().ok();
        }
    }
    None
}

/// Parse silence start and end times from silencedetect stderr.
/// Returns (silence_start_s, silence_end_s) where silence_end_s is seconds of
/// trailing silence (from end of file) and silence_start_s is leading silence.
fn parse_silence(text: &str, duration_s: f64) -> (f64, f64) {
    let mut first_end: Option<f64> = None; // first silence_end = end of leading silence
    let mut last_start: Option<f64> = None; // last silence_start = start of trailing silence

    for line in text.lines() {
        if line.contains("silence_start:") {
            if let Some(v) = line.split("silence_start:").nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<f64>().ok())
            {
                last_start = Some(v);
            }
        }
        if line.contains("silence_end:") {
            if let Some(v) = line.split("silence_end:").nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<f64>().ok())
            {
                if first_end.is_none() { first_end = Some(v); }
            }
        }
    }

    let silence_start_s = first_end.unwrap_or(0.0);
    let silence_end_s = last_start
        .map(|start| (duration_s - start).max(0.0))
        .unwrap_or(0.0);

    (silence_start_s, silence_end_s)
}

/// Confine an audio tool's input (read) + output (write) paths to the workspace,
/// returning canonical path strings for ffmpeg — or a clear tool error. Roots a
/// relative path in the (per-agent) workspace and turns an out-of-workspace target
/// into a readable "outside workspace" message instead of an opaque ffmpeg EROFS
/// under the read-only systemd sandbox (ProtectSystem=strict). (#6)
fn confine_audio_io(in_path: &str, out_path: &str) -> Result<(String, String), Value> {
    let i = confine(in_path, false).map_err(tool_error)?;
    let o = confine(out_path, true).map_err(tool_error)?;
    Ok((i.to_string_lossy().into_owned(), o.to_string_lossy().into_owned()))
}

fn audio_analyze(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return tool_error("path is required"),
    };
    // Confine the read to the workspace (+ read allowlist); roots a relative path.
    let path = match confine(path, false) { Ok(p) => p, Err(e) => return tool_error(e) };
    match audio_analyze_inner(&path.to_string_lossy()) {
        Ok(stats) => tool_ok(stats),
        Err(e) => tool_error(e),
    }
}

fn audio_trim_silence(args: &Value) -> Value {
    let path = match args["path"].as_str() { Some(p) => p, None => return tool_error("path required") };
    let out  = match args["output_path"].as_str() { Some(p) => p, None => return tool_error("output_path required") };
    let (path, out) = match confine_audio_io(path, out) { Ok(v) => v, Err(e) => return e };

    let trim_start = args["start"].as_bool().unwrap_or(true);
    let trim_end   = args["end"].as_bool().unwrap_or(true);
    let thresh_db  = args["threshold_db"].as_f64().unwrap_or(-50.0);
    let min_ms     = args["min_silence_ms"].as_f64().unwrap_or(500.0);
    let min_dur    = min_ms / 1000.0;

    let mut parts: Vec<String> = Vec::new();
    if trim_start {
        parts.push(format!(
            "silenceremove=start_periods=1:start_threshold={thresh_db}dB:start_duration={min_dur}"
        ));
    }
    if trim_end {
        parts.push(format!(
            "silenceremove=stop_periods=-1:stop_threshold={thresh_db}dB:stop_duration={min_dur}"
        ));
    }
    if parts.is_empty() {
        return tool_error("at least one of start or end must be true");
    }

    let filter = parts.join(",");
    let (_, stderr, ok) = cmd_capture("ffmpeg", &["-y", "-i", &path, "-af", &filter, &out]);
    if ok {
        tool_ok(json!({ "output_path": out }))
    } else {
        tool_error(format!("ffmpeg error: {}", stderr.lines().last().unwrap_or("")))
    }
}

fn audio_normalize(args: &Value) -> Value {
    let path        = match args["path"].as_str() { Some(p) => p, None => return tool_error("path required") };
    let out         = match args["output_path"].as_str() { Some(p) => p, None => return tool_error("output_path required") };
    let (path, out) = match confine_audio_io(path, out) { Ok(v) => v, Err(e) => return e };
    let target_lufs = args["target_lufs"].as_f64().unwrap_or(-14.0);
    let true_peak   = args["true_peak"].as_f64().unwrap_or(-2.0);

    // Pass 1: measure
    let filter1 = format!("loudnorm=I={target_lufs}:TP={true_peak}:LRA=11:print_format=json");
    let (_, stderr1, _) = cmd_capture("ffmpeg", &["-i", &path, "-af", &filter1, "-f", "null", "-"]);

    let measured = extract_json_from_text(&stderr1).unwrap_or_default();
    let mi    = measured["input_i"].as_str().unwrap_or("-70");
    let mtp   = measured["input_tp"].as_str().unwrap_or("-99");
    let mlra  = measured["input_lra"].as_str().unwrap_or("7");
    let mth   = measured["input_thresh"].as_str().unwrap_or("-80");
    let off   = measured["target_offset"].as_str().unwrap_or("0");

    // Pass 2: apply
    let filter2 = format!(
        "loudnorm=I={target_lufs}:TP={true_peak}:LRA=11:measured_I={mi}:measured_TP={mtp}:measured_LRA={mlra}:measured_thresh={mth}:offset={off}:linear=true"
    );
    let (_, stderr2, ok) = cmd_capture("ffmpeg", &["-y", "-i", &path, "-af", &filter2, &out]);

    if ok {
        tool_ok(json!({ "output_path": out, "measured_lufs": mi, "measured_peak": mtp }))
    } else {
        tool_error(format!("ffmpeg error: {}", stderr2.lines().last().unwrap_or("")))
    }
}

fn audio_peak_limit(args: &Value) -> Value {
    let path     = match args["path"].as_str() { Some(p) => p, None => return tool_error("path required") };
    let out      = match args["output_path"].as_str() { Some(p) => p, None => return tool_error("output_path required") };
    let (path, out) = match confine_audio_io(path, out) { Ok(v) => v, Err(e) => return e };
    let limit_db = args["limit_db"].as_f64().unwrap_or(-1.0);

    // Convert dBFS to linear (alimiter limit is linear 0..1)
    let limit_linear = 10f64.powf(limit_db / 20.0);
    let filter = format!("alimiter=limit={limit_linear:.4}:level_in=1:level_out=1:attack=5:release=50:asc=1");

    let (_, stderr, ok) = cmd_capture("ffmpeg", &["-y", "-i", &path, "-af", &filter, &out]);
    if ok {
        tool_ok(json!({ "output_path": out }))
    } else {
        tool_error(format!("ffmpeg error: {}", stderr.lines().last().unwrap_or("")))
    }
}

fn audio_trim(args: &Value) -> Value {
    let path  = match args["path"].as_str() { Some(p) => p, None => return tool_error("path required") };
    let out   = match args["output_path"].as_str() { Some(p) => p, None => return tool_error("output_path required") };
    let (path, out) = match confine_audio_io(path, out) { Ok(v) => v, Err(e) => return e };
    let start = args["start_s"].as_f64().unwrap_or(0.0);
    let end   = match args["end_s"].as_f64() { Some(e) => e, None => return tool_error("end_s required") };

    let start_str = format!("{start:.3}");
    let end_str   = format!("{end:.3}");
    // -c copy avoids re-encode; -ss/-to after -i for sample-accurate trim
    let (_, stderr, ok) = cmd_capture("ffmpeg", &[
        "-y", "-i", &path, "-ss", &start_str, "-to", &end_str, "-c", "copy", &out,
    ]);
    if ok {
        tool_ok(json!({ "output_path": out }))
    } else {
        tool_error(format!("ffmpeg error: {}", stderr.lines().last().unwrap_or("")))
    }
}

fn audio_clean(args: &Value) -> Value {
    let path = match args["path"].as_str() { Some(p) => p, None => return tool_error("path required") };
    let target_lufs = args["target_lufs"].as_f64().unwrap_or(-14.0);
    let thresh_db   = args["silence_threshold_db"].as_f64().unwrap_or(-50.0);

    // Confine the input (read); a relative path roots in the (per-agent) workspace.
    let path = match confine(path, false) { Ok(p) => p.to_string_lossy().into_owned(), Err(e) => return tool_error(e) };

    // Output: explicit output_path, else <stem>_clean.<ext> beside the (confined)
    // input. Either way confine it (write) → a clear "outside workspace" error, never
    // an opaque ffmpeg EROFS under the read-only sandbox.
    let out_raw = match args["output_path"].as_str() {
        Some(p) => p.to_string(),
        None => {
            let p = std::path::Path::new(&path);
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
            let ext  = p.extension().and_then(|s| s.to_str()).unwrap_or("mp3");
            let dir  = p.parent().and_then(|d| d.to_str()).unwrap_or(".");
            format!("{dir}/{stem}_clean.{ext}")
        }
    };
    let out = match confine(&out_raw, true) { Ok(p) => p.to_string_lossy().into_owned(), Err(e) => return tool_error(e) };

    // Analyze original
    let stats_before = match audio_analyze_inner(&path) {
        Ok(s) => s,
        Err(e) => return tool_error(format!("analyze failed: {e}")),
    };

    let peak_db         = stats_before["peak_db"].as_f64().unwrap_or(-99.0);
    let lufs            = stats_before["lufs_integrated"].as_f64().unwrap_or(-99.0);
    let silence_end_s   = stats_before["silence_end_s"].as_f64().unwrap_or(0.0);

    let mut ops_applied: Vec<&str> = Vec::new();
    let mut current_input = path.clone();
    let mut tmp_files: Vec<String> = Vec::new();

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    // Step 1: trim trailing silence if > 0.3s
    if silence_end_s > 0.3 {
        let tmp = format!("/tmp/apex_audio_{stamp}_trim.mp3");
        let min_dur = 0.5f64;
        let filter = format!(
            "silenceremove=stop_periods=-1:stop_threshold={thresh_db}dB:stop_duration={min_dur}"
        );
        let (_, _, ok) = cmd_capture("ffmpeg", &["-y", "-i", &current_input, "-af", &filter, &tmp]);
        if ok {
            tmp_files.push(tmp.clone());
            current_input = tmp;
            ops_applied.push("trim_silence");
        }
    }

    // Step 2: normalize if LUFS outside [target-2, target+1]
    if lufs < target_lufs - 2.0 || lufs > target_lufs + 1.0 {
        let tmp = format!("/tmp/apex_audio_{stamp}_norm.mp3");
        // Pass 1
        let f1 = format!("loudnorm=I={target_lufs}:TP=-2:LRA=11:print_format=json");
        let (_, stderr1, _) = cmd_capture("ffmpeg", &["-i", &current_input, "-af", &f1, "-f", "null", "-"]);
        let measured = extract_json_from_text(&stderr1).unwrap_or_default();
        let mi  = measured["input_i"].as_str().unwrap_or("-70");
        let mtp = measured["input_tp"].as_str().unwrap_or("-99");
        let mlra= measured["input_lra"].as_str().unwrap_or("7");
        let mth = measured["input_thresh"].as_str().unwrap_or("-80");
        let off = measured["target_offset"].as_str().unwrap_or("0");
        // Pass 2
        let f2 = format!(
            "loudnorm=I={target_lufs}:TP=-2:LRA=11:measured_I={mi}:measured_TP={mtp}:measured_LRA={mlra}:measured_thresh={mth}:offset={off}:linear=true"
        );
        let (_, _, ok) = cmd_capture("ffmpeg", &["-y", "-i", &current_input, "-af", &f2, &tmp]);
        if ok {
            tmp_files.push(tmp.clone());
            current_input = tmp;
            ops_applied.push("normalize");
        }
    }

    // Step 3: peak limit if peak > -1 dB
    if peak_db > -1.0 {
        let tmp = format!("/tmp/apex_audio_{stamp}_lim.mp3");
        let limit_linear = 10f64.powf(-1.0f64 / 20.0);
        let filter = format!("alimiter=limit={limit_linear:.4}:level_in=1:level_out=1:attack=5:release=50:asc=1");
        let (_, _, ok) = cmd_capture("ffmpeg", &["-y", "-i", &current_input, "-af", &filter, &tmp]);
        if ok {
            tmp_files.push(tmp.clone());
            current_input = tmp;
            ops_applied.push("peak_limit");
        }
    }

    // Copy final result to output path
    if current_input != out {
        let (_, stderr, ok) = cmd_capture("ffmpeg", &["-y", "-i", &current_input, "-c", "copy", &out]);
        if !ok {
            for t in &tmp_files { let _ = std::fs::remove_file(t); }
            return tool_error(format!("final copy failed: {}", stderr.lines().last().unwrap_or("")));
        }
    }

    // Cleanup tmp files
    for t in &tmp_files {
        let _ = std::fs::remove_file(t);
    }

    // Analyze output
    let stats_after = audio_analyze_inner(&out).unwrap_or_default();

    tool_ok(json!({
        "output_path":  out,
        "ops_applied":  ops_applied,
        "stats_before": stats_before,
        "stats_after":  stats_after,
    }))
}

// ─── GPIO ─────────────────────────────────────────────────────────────────────

// Pins reserved by default — I2C bus 1 (sensor head) + HAT EEPROM
const GPIO_RESERVED: &[(u32, &str)] = &[
    (0,  "I2C ID EEPROM (HAT standard)"),
    (1,  "I2C ID EEPROM (HAT standard)"),
    (2,  "I2C1 SDA — sensor head (BME688/MLX90640)"),
    (3,  "I2C1 SCL — sensor head (BME688/MLX90640)"),
    (27, "HAT ID EEPROM SD"),
    (28, "HAT ID EEPROM SC"),
];

fn gpio_reserved_check(gpio: u32) -> Option<&'static str> {
    // Allow override via APEX_GPIO_RESERVED=none env var
    if std::env::var("APEX_GPIO_RESERVED").as_deref() == Ok("none") {
        return None;
    }
    GPIO_RESERVED.iter().find(|(n, _)| *n == gpio).map(|(_, reason)| *reason)
}

fn gpio_detect_model() -> String {
    std::fs::read_to_string("/proc/device-tree/model")
        .unwrap_or_default()
        .trim_matches('\0')
        .trim()
        .to_string()
}

fn gpio_chip_path() -> String {
    if gpio_detect_model().contains("Raspberry Pi 5") {
        "/dev/gpiochip4".to_string()
    } else {
        "/dev/gpiochip0".to_string()
    }
}

// Returns the sysfs GPIO base number for the main 40-pin header chip.
// Pi 5: gpiochip4 → base 512. Pi 3/4: gpiochip0 → base 0.
fn gpio_sysfs_base() -> u32 {
    let chip = gpio_chip_path();
    let name = chip.trim_start_matches("/dev/");
    std::fs::read_to_string(format!("/sys/class/gpio/{}/base", name))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

// Export a GPIO via sysfs and return its path, or an error string.
fn gpio_sysfs_export(gpio: u32) -> Result<String, String> {
    let sysfs_n = gpio_sysfs_base() + gpio;
    let path = format!("/sys/class/gpio/gpio{}", sysfs_n);
    if !std::path::Path::new(&path).exists() {
        std::fs::write("/sys/class/gpio/export", sysfs_n.to_string())
            .map_err(|e| format!("export GPIO {}: {}", gpio, e))?;
        // Small settle delay after export
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(path)
}

fn gpio_info() -> Value {
    let model = gpio_detect_model();
    let chip  = gpio_chip_path();
    let base  = gpio_sysfs_base();
    let gpioget_ok = std::process::Command::new("gpioget").arg("--version")
        .output().map(|o| o.status.success()).unwrap_or(false);
    let reserved: Vec<_> = GPIO_RESERVED.iter()
        .map(|(n, r)| json!({ "gpio": n, "reason": r }))
        .collect();
    tool_ok(json!({
        "model":           model,
        "chip":            chip,
        "sysfs_base":      base,
        "gpioget_available": gpioget_ok,
        "reserved_pins":   reserved,
        "note": "Set APEX_GPIO_RESERVED=none to bypass reserved-pin checks (unsafe with sensor head)"
    }))
}

fn gpio_read(args: &Value) -> Value {
    let gpio = match args["gpio"].as_u64() {
        Some(n) => n as u32,
        None => return tool_error("gpio required"),
    };
    if let Some(reason) = gpio_reserved_check(gpio) {
        return tool_error(format!("GPIO {} is reserved: {}", gpio, reason));
    }
    let chip = gpio_chip_path();
    let offset = gpio.to_string();
    let (stdout, stderr, ok) = cmd_capture("gpioget", &[&chip, &offset]);
    if !ok {
        return tool_error(format!("gpioget failed: {}", stderr.trim()));
    }
    let value: u8 = stdout.trim().parse().unwrap_or(0);
    tool_ok(json!({ "gpio": gpio, "value": value }))
}

fn gpio_write(args: &Value) -> Value {
    let gpio = match args["gpio"].as_u64() {
        Some(n) => n as u32,
        None => return tool_error("gpio required"),
    };
    let value = match args["value"].as_u64() {
        Some(n) if n <= 1 => n as u8,
        Some(_) => return tool_error("value must be 0 or 1"),
        None => return tool_error("value required"),
    };
    if let Some(reason) = gpio_reserved_check(gpio) {
        return tool_error(format!("GPIO {} is reserved: {}", gpio, reason));
    }
    let path = match gpio_sysfs_export(gpio) {
        Ok(p) => p,
        Err(e) => return tool_error(e),
    };
    if let Err(e) = std::fs::write(format!("{}/direction", path), "out") {
        return tool_error(format!("set direction: {}", e));
    }
    if let Err(e) = std::fs::write(format!("{}/value", path), value.to_string()) {
        return tool_error(format!("write value: {}", e));
    }
    tool_ok(json!({ "gpio": gpio, "value": value, "ok": true }))
}

fn gpio_pulse(args: &Value) -> Value {
    let gpio = match args["gpio"].as_u64() {
        Some(n) => n as u32,
        None => return tool_error("gpio required"),
    };
    let duration_ms = args["duration_ms"].as_u64().unwrap_or(100);
    if let Some(reason) = gpio_reserved_check(gpio) {
        return tool_error(format!("GPIO {} is reserved: {}", gpio, reason));
    }
    let path = match gpio_sysfs_export(gpio) {
        Ok(p) => p,
        Err(e) => return tool_error(e),
    };
    let dir_path = format!("{}/direction", path);
    let val_path = format!("{}/value", path);
    if let Err(e) = std::fs::write(&dir_path, "out") {
        return tool_error(format!("set direction: {}", e));
    }
    let _ = std::fs::write(&val_path, "1");
    std::thread::sleep(std::time::Duration::from_millis(duration_ms));
    let _ = std::fs::write(&val_path, "0");
    tool_ok(json!({ "gpio": gpio, "duration_ms": duration_ms, "ok": true }))
}

// Find the sysfs pwmchipN path and channel for a given BCM GPIO.
// Pi 5 (RP1): GPIO 12→ch0, 13→ch1, 18→ch2, 19→ch3 under the RP1 PWM chip.
// Pi 4 (BCM2711): GPIO 12→ch0, 13→ch1, 18→ch0, 19→ch1 under pwmchip0.
// Returns (chip_path, channel) or an error string.
fn pwm_chip_for_gpio(gpio: u32) -> Result<(String, u32), String> {
    let pi5 = gpio_detect_model().contains("Raspberry Pi 5");
    let channel = match (pi5, gpio) {
        (true,  12) => 0,
        (true,  13) => 1,
        (true,  18) => 2,
        (true,  19) => 3,
        (false, 12) | (false, 18) => 0,
        (false, 13) | (false, 19) => 1,
        _ => return Err(format!("GPIO {} does not support hardware PWM (use 12, 13, 18, or 19)", gpio)),
    };
    // Scan /sys/class/pwm/ for a chip that has enough channels
    let pwm_dir = std::path::Path::new("/sys/class/pwm");
    if !pwm_dir.exists() {
        return Err("PWM sysfs not available — add dtoverlay=pwm-2chan to /boot/firmware/config.txt and reboot".to_string());
    }
    let entries: Vec<_> = std::fs::read_dir(pwm_dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("pwmchip"))
        .collect();
    if entries.is_empty() {
        return Err("no PWM chips found — add dtoverlay=pwm-2chan to /boot/firmware/config.txt and reboot".to_string());
    }
    // On Pi 5, the RP1 PWM chip has 4 channels; on Pi 4 it has 2.
    // Pick the chip with enough channels.
    let needed = channel + 1;
    for entry in &entries {
        let chip_path = entry.path().to_string_lossy().to_string();
        let npwm_path = format!("{}/npwm", chip_path);
        if let Ok(n) = std::fs::read_to_string(&npwm_path).map(|s| s.trim().parse::<u32>().unwrap_or(0)) {
            if n >= needed {
                return Ok((chip_path, channel));
            }
        }
    }
    // Fallback: use the first chip
    let chip_path = entries[0].path().to_string_lossy().to_string();
    Ok((chip_path, channel))
}

fn pwm_set(gpio: u32, freq_hz: f64, duty_pct: f64) -> Result<(), String> {
    let (chip_path, channel) = pwm_chip_for_gpio(gpio)?;
    let export_path = format!("{}/export", chip_path);
    let pwm_path    = format!("{}/pwm{}", chip_path, channel);

    // Export channel if not already exported
    if !std::path::Path::new(&pwm_path).exists() {
        std::fs::write(&export_path, channel.to_string())
            .map_err(|e| format!("PWM export: {}", e))?;
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Disable before changing period (kernel requirement)
    let _ = std::fs::write(format!("{}/enable", pwm_path), "0");

    let period_ns = (1_000_000_000.0 / freq_hz) as u64;
    let duty_ns   = ((duty_pct / 100.0) * period_ns as f64) as u64;

    std::fs::write(format!("{}/period", pwm_path), period_ns.to_string())
        .map_err(|e| format!("set period: {}", e))?;
    std::fs::write(format!("{}/duty_cycle", pwm_path), duty_ns.to_string())
        .map_err(|e| format!("set duty_cycle: {}", e))?;
    std::fs::write(format!("{}/enable", pwm_path), "1")
        .map_err(|e| format!("enable PWM: {}", e))?;
    Ok(())
}

fn gpio_pwm(args: &Value) -> Value {
    let gpio = match args["gpio"].as_u64() {
        Some(n) => n as u32,
        None => return tool_error("gpio required"),
    };
    let duty_pct = args["duty_pct"].as_f64().unwrap_or(0.0).clamp(0.0, 100.0);
    let freq_hz  = args["freq_hz"].as_f64().unwrap_or(1000.0);
    if let Some(reason) = gpio_reserved_check(gpio) {
        return tool_error(format!("GPIO {} is reserved: {}", gpio, reason));
    }
    match pwm_set(gpio, freq_hz, duty_pct) {
        Ok(()) => tool_ok(json!({ "gpio": gpio, "freq_hz": freq_hz, "duty_pct": duty_pct, "ok": true })),
        Err(e) => tool_error(e),
    }
}

fn gpio_servo(args: &Value) -> Value {
    let gpio = match args["gpio"].as_u64() {
        Some(n) => n as u32,
        None => return tool_error("gpio required"),
    };
    let angle = args["angle_deg"].as_f64().unwrap_or(90.0).clamp(0.0, 180.0);
    if let Some(reason) = gpio_reserved_check(gpio) {
        return tool_error(format!("GPIO {} is reserved: {}", gpio, reason));
    }
    // Standard servo: 50Hz, 1ms (5% duty) = 0°, 1.5ms (7.5%) = 90°, 2ms (10%) = 180°
    let freq_hz  = 50.0_f64;
    let duty_pct = 5.0 + (angle / 180.0) * 5.0; // 5%–10% at 50Hz
    match pwm_set(gpio, freq_hz, duty_pct) {
        Ok(()) => tool_ok(json!({ "gpio": gpio, "angle_deg": angle, "duty_pct": duty_pct, "freq_hz": freq_hz, "ok": true })),
        Err(e) => tool_error(e),
    }
}

fn display_face(args: &Value) -> Value {
    // Activity states are normally driven by the event stream; emotive states are
    // what the agent reaches for. Both accepted. The Slint UI renders the face from
    // the tool_requested event it already receives — this tool's job is to validate
    // + echo (and, on the original Pi, poke the GC9A01A daemon best-effort).
    let state = args["state"].as_str().unwrap_or("neutral");
    let valid = [
        "idle", "thinking", "speaking", "listening", "alert", "sleeping",     // activity
        "neutral", "happy", "curious", "amused", "confused", "sad",           // emotive
        "surprised", "wink", "skeptical", "proud", "love", "focused",
    ];
    if !valid.contains(&state) {
        return tool_error(format!("invalid state '{}' — use one of: {}", state, valid.join(", ")));
    }
    let gaze = match args["gaze"].as_str().unwrap_or("center") {
        g @ ("center" | "left" | "right" | "up" | "down") => g,
        _ => "center",
    };
    let intensity = (args["intensity"].as_f64().unwrap_or(0.7)).clamp(0.0, 1.0);
    let text = args["text"].as_str().unwrap_or("");

    // Best-effort poke of the original ApexOS GC9A01A round-TFT daemon, if this
    // node happens to have one. Its absence is normal on -RS (the Slint face is
    // driven UI-side from the event) — so a missing/closed socket is NOT an error.
    let sock_path = "/run/apex-face/face.sock";
    if std::path::Path::new(sock_path).exists() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        if let Ok(mut stream) = UnixStream::connect(sock_path) {
            let msg = format!("{}\n", json!({ "state": state, "text": text }));
            let _ = stream.write_all(msg.as_bytes());
        }
    }

    // Always ok: the expression request is the contract; rendering is the UI's job.
    tool_ok(json!({ "ok": true, "state": state, "gaze": gaze, "intensity": intensity }))
}

fn sketch_draw(args: &Value) -> Value {
    // The Slint UI renders these strokes from the tool_requested event it already
    // receives (mirrors display_face) — scaling the normalized coords to its
    // canvas and persisting a composite PNG. This handler has no canvas of its
    // own; it validates the request + acks. A headless node simply has no
    // sketchpad, which is a no-op, not an error.
    let strokes = match args["strokes"].as_array() {
        Some(s) if !s.is_empty() => s,
        _ => return tool_error("sketch_draw: `strokes` must be a non-empty array".to_string()),
    };
    let clear = args["clear"].as_bool().unwrap_or(false);

    // Each stroke is valid if it carries a `points` path OR a `shape` with
    // `from`+`to`. Count the usable ones so the ack is honest.
    let mut n_ok = 0usize;
    for s in strokes {
        let has_points = s["points"].as_array().map(|p| !p.is_empty()).unwrap_or(false);
        let has_shape  = s["shape"].as_str().is_some()
            && s["from"].as_array().is_some()
            && s["to"].as_array().is_some();
        if has_points || has_shape { n_ok += 1; }
    }
    if n_ok == 0 {
        return tool_error(
            "sketch_draw: no valid strokes — each needs `points` or `shape`+`from`+`to`".to_string(),
        );
    }

    tool_ok(json!({
        "ok": true,
        "strokes": n_ok,
        "cleared": clear,
        "note": "Dispatched to the Sketchpad. If this node has a display, it renders on the live canvas — call sketch_snapshot to view the result.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    // Serialize tests that mutate AGENTD_WORKSPACE — env vars are process-global
    // and Rust runs tests in parallel by default.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn eject_label_validation_matches_gateway() {
        // Accept: APEX- prefix, sane single component.
        assert!(valid_exo_label("APEX-config"));
        assert!(valid_exo_label("APEX-work_2024.1"));
        // Reject: wrong prefix, escapes, separators, length, bad chars — so the
        // sudo umount helper is never handed a crafted/widened target.
        assert!(!valid_exo_label("config"));            // no APEX- prefix
        assert!(!valid_exo_label("APEX-"));             // empty name
        assert!(!valid_exo_label("APEX-a/b"));          // path separator
        assert!(!valid_exo_label("APEX-../etc"));       // traversal
        assert!(!valid_exo_label("APEX-a b"));          // space
        assert!(!valid_exo_label("APEX-$(x)"));         // shell-ish chars
        assert!(!valid_exo_label(&format!("APEX-{}", "x".repeat(70)))); // too long
    }

    #[test]
    fn disk_usage_skips_pseudo_at_boundary_and_picks_longest_mount() {
        // Pseudo skip matches at a path boundary — a real mount sharing a prefix stays.
        assert!(is_pseudo_mount("/dev"));
        assert!(is_pseudo_mount("/dev/shm"));
        assert!(is_pseudo_mount("/proc"));
        assert!(is_pseudo_mount("/run/user/1000"));
        assert!(!is_pseudo_mount("/devel"));     // the bare-prefix bug
        assert!(!is_pseudo_mount("/home"));
        assert!(!is_pseudo_mount("/system"));

        // The holding filesystem = the longest matching mountpoint, boundary-aware.
        let mounts = ["/", "/home", "/home/user/data"];
        assert_eq!(best_mount_for(&mounts, "/home/user/data/x"), Some("/home/user/data"));
        assert_eq!(best_mount_for(&mounts, "/home/bob"), Some("/home"));
        assert_eq!(best_mount_for(&mounts, "/etc/passwd"), Some("/"));
        assert_eq!(best_mount_for(&mounts, "/homestead"), Some("/")); // not /home
        assert_eq!(best_mount_for(&mounts, "/home"), Some("/home"));   // exact
        assert_eq!(best_mount_for(&["/data"], "/etc"), None);          // nothing holds it
    }

    #[test]
    fn sanitize_note_name_forces_md_and_blocks_traversal() {
        // Stem gets a .md extension; an existing .md is not doubled.
        assert_eq!(sanitize_note_name("ideas").as_deref(), Some("ideas.md"));
        assert_eq!(sanitize_note_name("ideas.md").as_deref(), Some("ideas.md"));
        assert_eq!(sanitize_note_name("  spaced  ").as_deref(), Some("spaced.md"));
        // Path components are stripped → no traversal escapes the notes dir.
        assert_eq!(sanitize_note_name("../../etc/passwd").as_deref(), Some("passwd.md"));
        assert_eq!(sanitize_note_name("/abs/secret.md").as_deref(), Some("secret.md"));
        // Nothing usable → None.
        assert_eq!(sanitize_note_name(""), None);
        assert_eq!(sanitize_note_name("   "), None);
        assert_eq!(sanitize_note_name(".."), None);
        assert_eq!(sanitize_note_name(".md"), None);
    }

    #[test]
    fn resolve_path_relative_vs_absolute() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Absolute paths pass through unchanged regardless of workspace.
        std::env::set_var("AGENTD_WORKSPACE", "/srv/ws");
        assert_eq!(resolve_path("/etc/hosts"), Path::new("/etc/hosts"));

        // Relative paths root onto AGENTD_WORKSPACE.
        assert_eq!(resolve_path("notes.txt"), Path::new("/srv/ws/notes.txt"));
        assert_eq!(resolve_path("a/b.txt"), Path::new("/srv/ws/a/b.txt"));

        // Empty workspace falls back to the default root.
        std::env::set_var("AGENTD_WORKSPACE", "");
        assert_eq!(
            resolve_path("notes.txt"),
            Path::new("/var/lib/agentd/workspace/notes.txt")
        );

        // Unset workspace falls back to the default root.
        std::env::remove_var("AGENTD_WORKSPACE");
        assert_eq!(
            resolve_path("notes.txt"),
            Path::new("/var/lib/agentd/workspace/notes.txt")
        );
        // Absolute still passes through with no workspace set.
        assert_eq!(resolve_path("/tmp/x"), Path::new("/tmp/x"));
    }

    #[test]
    fn confine_enforces_workspace_and_read_allowlist() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AGENTD_WORKSPACE", "/tmp");

        // Inside the workspace: reads and writes both pass (write target need
        // not exist — only the parent has to resolve).
        assert!(confine("note.txt", false).is_ok());
        assert!(confine("/tmp/sub/out.bin", true).is_ok());

        // Writes are workspace-only: an absolute path outside is blocked.
        assert!(confine("/etc/cron.d/x", true).is_err());

        // `..` traversal is rejected for both reads and writes.
        assert!(confine("/tmp/../etc/passwd", false).is_err());
        assert!(confine("../escape.txt", true).is_err());

        // Secret-exfil vectors are never readable (the bug this fix closes).
        assert!(confine("/proc/self/environ", false).is_err());
        assert!(confine("/etc/shadow", false).is_err());

        // Read allowlist: the EDK on-hand inventory is readable though it lives
        // outside the workspace — but it is NOT writable (writes stay ws-only).
        assert!(confine("/etc/agentd/parts/inventory.toml", false).is_ok());
        assert!(confine("/etc/agentd/parts/inventory.toml", true).is_err());

        // Default-deny: a non-allowlisted, non-workspace read is refused.
        assert!(confine("/etc/hostname", false).is_err());

        // Operator can widen reads via AGENTD_READ_ROOTS without a recompile.
        std::env::set_var("AGENTD_READ_ROOTS", "/etc/hostname");
        assert!(confine("/etc/hostname", false).is_ok());
        std::env::remove_var("AGENTD_READ_ROOTS");

        std::env::remove_var("AGENTD_WORKSPACE");
    }

    #[test]
    fn audio_output_outside_workspace_rejected_clearly() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AGENTD_WORKSPACE", "/tmp");

        // An absolute output outside the workspace is refused with a clear
        // "outside workspace" message BEFORE ffmpeg runs — not an opaque EROFS (#6).
        let res = audio_trim(&json!({
            "path": "in.wav", "output_path": "/etc/evil.wav", "end_s": 1.0,
        }));
        assert_eq!(res["isError"], json!(true));
        let txt = res["content"][0]["text"].as_str().unwrap_or("");
        assert!(txt.contains("workspace"), "expected a workspace-confinement error, got: {txt}");

        // A `..` escape on the output is likewise rejected (not passed to ffmpeg).
        let res2 = audio_normalize(&json!({ "path": "in.wav", "output_path": "../escape.wav" }));
        assert_eq!(res2["isError"], json!(true));

        std::env::remove_var("AGENTD_WORKSPACE");
    }

    #[test]
    fn confine_is_per_agent_when_workspace_stamped() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A global env the per-call stamp must override (proves the stamp wins).
        std::env::set_var("AGENTD_WORKSPACE", "/tmp");

        let base  = std::path::Path::new("/tmp/apexos-pa-ws");
        let luma  = base.join("workspaces").join("LUMA");
        let probe = base.join("workspaces").join("PROBE");
        let _ = fs::create_dir_all(&luma);
        let _ = fs::create_dir_all(&probe);

        // Pin LUMA's stamped workspace for the calls below (cleared on drop).
        let _ws = WorkspaceGuard::enter(Some(luma.to_string_lossy().into_owned()));

        // The stamp wins over AGENTD_WORKSPACE: a relative path lands in LUMA's root.
        let resolved = confine("note.txt", true).expect("LUMA writes its own ws");
        assert!(resolved.starts_with(fs::canonicalize(&luma).unwrap()));

        // LUMA is sealed from a sibling agent's workspace — read and write both fail.
        let probe_secret = probe.join("secret.txt");
        let probe_s = probe_secret.to_string_lossy();
        assert!(confine(&probe_s, false).is_err(), "LUMA must not read PROBE's ws");
        assert!(confine(&probe_s, true).is_err(),  "LUMA must not write PROBE's ws");

        // `..` traversal out of LUMA's root is still rejected.
        assert!(confine("../PROBE/secret.txt", true).is_err());

        drop(_ws);
        // Back to the env fallback once unstamped: relative paths root onto /tmp.
        assert!(confine("note.txt", false).is_ok());

        let _ = fs::remove_dir_all(base);
        std::env::remove_var("AGENTD_WORKSPACE");
    }

    #[test]
    fn ssrf_blocks_private_and_loopback() {
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // IPv4-mapped loopback.
        assert!(is_blocked_ip(IpAddr::V6("::ffff:127.0.0.1".parse().unwrap())));
        // fe80:: link-local and fc00:: ULA.
        assert!(is_blocked_ip(IpAddr::V6("fe80::1".parse().unwrap())));
        assert!(is_blocked_ip(IpAddr::V6("fc00::1".parse().unwrap())));
    }

    #[test]
    fn ssrf_allows_public() {
        // 172.15 is NOT in 172.16/12; 8.8.8.8 is public.
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(172, 15, 0, 1))));
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!is_blocked_ip(IpAddr::V6("2606:4700:4700::1111".parse().unwrap())));
    }

    // ─── Git tools ────────────────────────────────────────────────────────────

    #[test]
    fn git_safe_arg_blocks_option_injection() {
        assert!(git_safe_arg("ref", "main").is_ok());
        assert!(git_safe_arg("ref", "HEAD~1").is_ok());
        assert!(git_safe_arg("ref", "feature/x").is_ok());
        // A leading dash would be read by git as an option, not a ref.
        assert!(git_safe_arg("ref", "--upload-pack=evil").is_err());
        assert!(git_safe_arg("branch", "-x").is_err());
    }

    #[test]
    fn confine_git_repo_workspace_and_roots() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = std::env::temp_dir().join("apexos_git_confine_ws");
        fs::create_dir_all(&ws).unwrap();
        std::env::set_var("AGENTD_WORKSPACE", &ws);
        std::env::remove_var("AGENTD_GIT_ROOTS");

        let ws_canon = fs::canonicalize(&ws).unwrap();
        // Default (`.`) resolves to the workspace.
        assert_eq!(confine_git_repo(None).unwrap(), ws_canon);
        // `..` is rejected.
        assert!(confine_git_repo(Some("../escape")).is_err());

        // An absolute path outside the workspace is rejected by default…
        let outside = std::env::temp_dir().join("apexos_git_confine_outside");
        fs::create_dir_all(&outside).unwrap();
        let outside_s = outside.to_str().unwrap().to_string();
        assert!(confine_git_repo(Some(&outside_s)).is_err());
        // …until the operator opts it in via AGENTD_GIT_ROOTS.
        std::env::set_var("AGENTD_GIT_ROOTS", &outside_s);
        assert!(confine_git_repo(Some(&outside_s)).is_ok());

        std::env::remove_var("AGENTD_GIT_ROOTS");
        std::env::remove_var("AGENTD_WORKSPACE");
        let _ = fs::remove_dir_all(&ws);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn git_init_commit_log_roundtrip() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Skip gracefully if git isn't installed (it is in CI/dev).
        if !cmd_capture("git", &["--version"]).2 {
            return;
        }
        let ws = std::env::temp_dir().join("apexos_git_roundtrip_ws");
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).unwrap();
        std::env::set_var("AGENTD_WORKSPACE", &ws);
        std::env::remove_var("AGENTD_GIT_ROOTS");

        let ok = |r: &Value| r.get("isError").is_none();
        // git_init in the workspace (default path).
        assert!(ok(&git_init(&json!({}))), "init failed");
        // Commit a new file (stage-all path).
        fs::write(ws.join("hello.txt"), "hi").unwrap();
        assert!(ok(&git_commit(&json!({ "message": "first commit" }))), "commit failed");
        // git_log surfaces the commit message.
        let r = git_log(&json!({ "n": 5 }));
        let inner: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(
            inner["output"].as_str().unwrap().contains("first commit"),
            "log missing commit: {inner}"
        );

        std::env::remove_var("AGENTD_WORKSPACE");
        let _ = fs::remove_dir_all(&ws);
    }
}
