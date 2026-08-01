# Imagine Studio arc — kiosk parity, video, and the cutting room

> The multi-slice charter (started 2026-07-29) for growing the native 🖼 Imagine app
> from image-first v1 into a **full studio at parity with Imaginarium's browser
> studio** — video generation, in-app playback, chain ergonomics, and a native
> video-editing "cutting room" with Sonus scoring. Low-cost / old-hardware kiosk
> nodes are the first-class target: everything a browser does, on a node that
> can't run one. Two repos move together: ApexOS-RS (the surface) and
> Imaginarium-RS (the engine). This doc is the arc's source of truth — slices
> tick here as they merge.

Prior art feeding the arc:

- **`~/Projects/cutting-room`** (MIT, André + Fable, distilled from the VIMANA
  trailer production — a full teaser cut end-to-end, one shot, agent-driven):
  a Claude-skill methodology + ffmpeg recipe book. Its engine architecture and
  recipes port to Rust inside Imaginarium's craft renderer; the **skill itself
  stays a skill** — it becomes APEX's editing methodology once the surface
  exists (edit-as-data was designed for an agent operator).
- The 2026-07-29 sweeps: web-studio feature inventory, craft-backend gaps, and
  the cutting-room portability verdicts (Cerebro session notes, FORGE).

## Starting state (post #289–#291 — superseded by the ledger below; the arc completed A1–A7)

Native app ships image generate + still preview + the shared jobs rail.
Video jobs render a "open the browser studio" placeholder. The browser studio
(`ui-web/`, on the node at :8791) has image gen/edit, video T2V/I2V/R2V/edit/
extend, a 7-edge ChainBar, a canvas image editor, and a minimal clip-list video
craft. The craft backend renders timelines but has **no audio track, no
normalization before concat, no thumbnails, data-URL-only media inputs**.

## The video player (locked design — A1)

Slint has no video element; we don't need one. The player is a hand-rolled
ffmpeg pipeline reusing three field-proven ApexOS idioms:

1. **Fetch-then-decode**: download `/v1/library/{id}/content` to
   `$XDG_CACHE_HOME/apexos-rs/imagine/` (clips are 2–20 MB; upstream has no
   Range support and doesn't need it for this).
2. **Frames**: `ffmpeg -i file -f rawvideo -pix_fmt rgba -vf scale=<win_w>:-2 -`
   on a decoder thread → bounded channel → Slint `Timer` pops a frame per tick
   into a `SharedPixelBuffer` (the thermal-heatmap idiom at 24–30 fps).
   **Decode at window size, never native res** — that's the old-hardware story.
3. **Audio**: a second ffmpeg (`-vn -f wav -`) piped into `aplay` (the
   client-side voice-arc idiom). Clips are 4–15 s: starting both pipelines
   together bounds drift to tens of ms — **no sync engine, on purpose**.

Degrade ladder: window-size decode → 15 fps tier → poster-frame + audio-only
(Nano/femtovg). **No gstreamer, no libav bindings** — ffmpeg CLI only (already
a node dependency via imaginarium provisioning, camera, and audio tools).

## Slice ledger

Interleaved PRs, each reviewable alone; André merges. Tick + link the PR here
as each lands.

| # | Repo | Slice | Status |
|---|------|-------|--------|
| A1 | ApexOS-RS | **Video player** in the Imagine app: fetch-to-cache, poster frame, play/stop/replay, progress line, the degrade ladder | ✅ #293 |
| U1 | Imaginarium | **Client ergonomics**: `library:{job_id}` MediaRef (kills download→base64 chains + the 40 MB chain ceiling), `?i=` multi-asset content addressing (n>1 batches reachable), jobs-list projection carrying prompt + first-asset kind | ✅ Imag#4 |
| A2 | ApexOS-RS | **Video generation**: T2V + I2V (source = library job via chain), duration/aspect/resolution chips, `no_wait` submit + watcher polling with etiquette-guarded auto-open, modality/model validation mirrored (1080p only when I2V) | ✅ #294 |
| A3 | ApexOS-RS | **ChainBar native**: result actions on the preview (image→ANIMATE, video→EXTEND; → Edit/→ Craft land with A4/A5), chain-source chip with clear, jobs rail shows U1's prompt projection | ✅ #294 (first edges) |
| A2b | ApexOS-RS | **Cinematic pipeline (T2I2V)**: ✨ CINEMA toggle — prompt → quality still → v1.5 animates it at 1080p (v1.5 is I2V-only upstream; André's field observation made one-click). Two visible spends, one button | ✅ #297 |
| U2a | Imaginarium | **Craft engine correctness** (the cutting-room port, part 1): per-segment normalize filter → concat, **master-clock single audio pass + music-bed audio track** (`AssetKind::Audio`, library audio sniff/mime/import/content-type), segment-owned captions (makes the lost-overlay bug unrepresentable), ffprobe durations, `-nostdin` + even-dimension pitfalls as tests | ✅ Imag#5 |
| U2b | Imaginarium | **Craft engine expressiveness** (part 2): versioned merged timeline contract (style block, segment kinds `clip`/`still`+Ken Burns/`card`, speed, letterbox recipes), two-pass loudnorm ship pass, content-hash segment caching, provenance field | ✅ Imag#6 |
| U3 | Imaginarium | **Thumbnails/posters** (`thumb.jpg` on completion via the contact-sheet recipe + `/v1/library/{id}/thumb`) + **async craft render** (job id immediately, poll like any job) | ✅ Imag#7 |
| A4 | ApexOS-RS | **Image edit flow**: prompt + up to 3 sources (library chain / workspace picker), riding U1's job MediaRef | ✅ #300 |
| A5 | ApexOS-RS | **The Cutting Room**: native timeline mode in the Imagine window — library picker with thumbnails, clip in/out/gain, segment captions, fades, style controls, render → job | ✅ #298 |
| A6 | ApexOS-RS | **Score with Sonus**: music-bed picker off `/api/sonus/files` + `/api/sonus/stream` → library audio import → timeline bed; plus "🎵 ask APEX to compose" firing a queued `user_prompt` (the occipital-steer idiom). Honest no-sonus degrade | ✅ #299 |
| A7 | — | **The agent story**: adapt the cutting-room skill so APEX drives the timeline API conversationally (procedure + soul evolution on the visual-artist node) | ✅ Imag#8 + `docs/imagine-craft-skill.md` |

## The merged timeline contract — LANDED as version 1 (U2a/U2b, 2026-07-29)

Shipped in Imaginarium `crates/imaginarium-core/src/craft_video.rs`; full schema
in its `openapi/imaginarium-v1.yaml` (`VideoTimeline`). The sketch's intent
landed with the **shipped U2a field names** (serde compat beat sketch spelling):

- `version: 1` (0/absent = legacy, identical semantics; newer → 400).
- Segments stay in `clips[]` with a `kind` field: `clip {job_id, in_s, out_s,
  speed 0.5–2.0, gain_db}` · `still {job_id, dur_s, zoom_from/zoom_to 1–3}`
  (Ken Burns, one frame → zoompan, never `-loop 1`) · `card {dur_s,
  card_color}`. Durations ffprobe-measured; master clock frame-quantized.
- **Captions are segment-owned with segment-local time** (`clips[].captions`);
  timeline `overlays` stay master-clock and map/split across every segment
  they intersect — the lost-overlay bug is unrepresentable either way.
- Music bed = top-level `music {job_id, in_s, start_s, gain_db, fades}` (not
  the sketched `audio.bed` nesting), mixed `amix normalize=0` on the master
  clock after the silent-segment concat.
- `style {caption_fontsize, caption_color, card_bg, letterbox_frac,
  letterbox_reveal_s, loudnorm}`; canvas stays top-level `width/height/fps`
  (0 = derive from first clip). Deltas vs sketch: no font-file selection yet
  (cross-node font paths = a later slice); letterbox is global style, not
  per-card.
- Provenance: the full submitted timeline + engine/ffmpeg versions ride the
  craft job's `meta.json`; segments are content-hash cached
  (`{data-home}/craft-segcache`, 2 GiB LRU).
- U3 riders: `?no_wait=true` async craft render (poll like any job; `/wait` on
  a craft job returns the DB row) and `GET /v1/library/{id}/thumb` 480px
  posters (eager on import, lazy backfill).
## Judgment calls (agreed 2026-07-29)

- **Parked**: the paint/mask image canvas (the browser keeps it — 899 lines of
  per-pixel canvas with no Slint primitive; revisit post-arc), R2V (multi-ref
  video), the Audacity script-pipe DSP rack (GUI-coupled; ffmpeg filters cover
  the need; `an3-audacity-mcp` exists if ever wanted), Range/206 streaming +
  library delete/tag/search (a later U4).
- Sonus v1 = **pick existing tracks**; generating new music stays
  conversational through APEX (sonus tools are agent-facing).
- The Cutting Room is a **mode inside the Imagine window**, not a 22nd app.
- Player is ffmpeg-CLI-only — adding gstreamer/libav bindings is a
  locked-decision-level change, don't.

## Field verification

Each A-slice field-tests on apex-3 (desktop) then apex1 (kiosk); the degrade
ladder specifically wants the weakest node available. The arc's closing test is
the full loop: APEX generates clips → human (or APEX) cuts them with a Sonus
bed in the native Cutting Room → the render lands in the shared library — on a
node with no browser.
