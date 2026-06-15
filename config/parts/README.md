# ApexOS parts catalog — curation guide

This directory is the **curated, human-verified** dataset of hardware an ApexOS node can
accept. It is the "map of reachable selves" half of the embodiment gradient — see
[`docs/edk.md`](../../docs/edk.md) for the why. The agent **reads** this; it never writes or
infers it. That is the rule: you cannot probe a part you don't own, so possible-bodies are
reference data, kept honest by humans.

- `catalog.toml` — the parts, one `[[part]]` table each. Start here.
- Split into category files later (`sense.toml`, `compute.toml`, …) if it grows; the loader
  will read every `*.toml` in this dir.

## How to add a part

1. Copy a `[[part]]` block of the same `category` from `catalog.toml`.
2. Fill every field. If you can't verify a value, leave it and set `status = "inferred"` or
   `"todo"` — **never invent a spec the agent will trust.** Honesty over completeness.
3. Set `unlocks_tools` to a tool that already exists in the registry, or `"new:<name>"` for a
   capability that still needs a plugin written.
4. Keep `id` a stable kebab-case slug — the hardware-wishlist references parts by `id`.

## Field schema

Every `[[part]]` carries:

| Field | Type | Meaning |
|---|---|---|
| `id` | slug | stable kebab-case key; wishlist + requests reference this. Never reuse/rename. |
| `name` | string | the human product name |
| `category` | enum | `sense` · `actuator` · `display` · `input` · `compute` · `power` · `connectivity` · `storage` · `hat` |
| `provides` | string | the capability in **agent terms** ("eyes", "hearing", "physical button input") |
| `summary` | string | one line: what it is and what it does for the agent |
| `bus` | enum | how it attaches: `csi` · `dsi` · `i2c` · `spi` · `uart` · `1wire` · `gpio` · `usb` · `m2-hat+` · `pcie` · `hdmi` · `audio` · `poe` · `rtc` · `fan` |
| `pins` | int[] | BCM/header pins it occupies (for conflict detection); `[]` if not a header part |
| `compat` | slug[] | hosts it works on: `pi5` · `pi4` · `pi3` · `zero2w` · `x86` … |
| `compat_notes` | string | **the seams** — where "a Pi is a Pi" breaks (see below). Empty if none. |
| `cost` | float | approximate price |
| `currency` | string | `USD` / `GBP` / … (PiHut is `GBP`) |
| `vendor` | string | where to buy |
| `vendor_sku` | string | vendor part number (for the future self-purchase loop) |
| `product_url` | string | link |
| `enable` | string[] | ordered checklist to bring it up: ribbon orientation, `dtoverlay=…`, `apt` pkgs, groups |
| `detect` | string | shell probe that proves it's physically present (`ls /dev/video*`, `i2cdetect -y 1`, …) |
| `detect_tool` | string | an existing tool whose success proves the part works end-to-end; `""` if none yet |
| `unlocks_tools` | string[] | tools that light up — existing names, or `"new:<name>"` for a plugin to be written |
| `power_draw` | string | rough current/notes (the Pi 5 current budget is real) |
| `status` | enum | `verified` (confirmed on real hardware) · `inferred` (best-effort, needs check) · `todo` (stub) |
| `notes` | string | the chef's-kiss gotcha detail |

## The seams — where "a Pi is a Pi" stops being true

The 40-pin GPIO header is identical across Pi 3 / 4 / 5 / Zero 2 W, so most HATs are
portable. But these differences will make an agent confidently wrong if `compat_notes`
doesn't capture them:

- **Camera/display connector** — Pi 5 uses a **22-pin FPC**; Pi 4 and earlier use **15-pin**.
  Different ribbon cable (or an adapter). A Pi 4 camera cable will not fit a Pi 5.
- **PCIe / M.2** — Pi 5 only (via the M.2 HAT+ / AI HAT+). Hailo accelerators and NVMe are
  **Pi 5 exclusive** — never mark them `compat = ["pi4"]`.
- **RP1 I/O controller** — Pi 5's GPIO goes through RP1; the old `RPi.GPIO` library breaks.
  Userspace GPIO needs `lgpio` / `gpiod`. Note this in `enable` for any GPIO part.
- **No analog audio jack** — Pi 5 dropped the 3.5 mm jack. Audio out = HDMI (`plughw:1,0`),
  a USB DAC, or an I2S/DAC HAT. "Add a speaker" is never "plug into the headphone jack."
- **Power budget** — Pi 5 wants a 5 V/5 A (PD) supply; power-hungry HATs + peripherals can
  brown out on a weaker PSU. Flag heavy draws in `power_draw`.

## Importing from a vendor (PiHut etc.)

The flow is **scrape → filter → curate**, never scrape-into-catalog:

1. Pull the vendor's catalog (PiHut carries nearly everything on stock).
2. Filter to parts that make sense for *extending an ApexOS agent* — senses, actuators,
   compute, the things that map to a capability. Drop the rest.
3. For each kept part, fill the schema above. Set `vendor`/`vendor_sku`/`product_url` from the
   listing; set `status = "inferred"` until confirmed on real hardware, `"verified"` once seen
   working via its `detect_tool`.
