# The Cutting Room skill — how an agent edits video on this node

> The cutting-room methodology (the MIT Claude-skill that cut the VIMANA
> trailer end-to-end), adapted for APEX agents driving Imaginarium's craft
> engine through MCP tools. The engine does the hard parts — normalization,
> master-clock audio, caching — so the skill is **taste + sequence**, not
> ffmpeg. Written to be stored as a Cerebro procedure on the visual-artist
> node and evolved from there (see *Make it yours*, below).

## When to reach for it

Someone wants a *piece* — a trailer, a montage, a titled clip, a scored short —
assembled from things that already exist (or that you generate first). One
clip needs no cutting room; two clips, a title, or music does.

## The tool surface (3 tools, everything else is judgment)

| Tool | Role |
|---|---|
| `imaginarium_jobs_list` | The inventory — what's in the library (id, mode, status, prompt, asset count) |
| `imaginarium_craft_video` | The cut — `{timeline, wait?: false}` → a pending craft job |
| `imaginarium_job_status` | The poll — craft jobs are DB-truth; loop until `done`/`failed` |

Generation tools (`imaginarium_image_generate`, `imaginarium_video_generate`,
…) fill the shelves first when the library is missing an ingredient. Renders
are **free and on-node**; generation is the part that costs money.

## The loop

1. **Inventory** — `imaginarium_jobs_list`, pick `done` jobs. Video jobs become
   clips, image jobs become Ken-Burns stills, imported audio becomes the bed.
2. **Plan the timeline** — write the v1 JSON (grammar below). Think in
   sentences: establish → develop → land. 10–30 seconds total is the sweet
   spot for generated footage.
3. **Render** — `imaginarium_craft_video {timeline}` (default `wait: false`).
4. **Poll** — `imaginarium_job_status` every few seconds until `done`. A
   `failed` job carries the real reason (bad trim window, missing media) —
   fix the timeline and resubmit; unchanged segments come from cache, so a
   re-render costs only the segment you touched.
5. **Deliver** — report the library job id. Humans preview it in the Imagine
   window's rail (▶ plays it) or the web studio; it's chainable like any job.

## The grammar (timeline contract v1)

```json
{
  "version": 1,
  "clips": [
    { "kind": "card", "dur_s": 2, "card_color": "#101418",
      "captions": [{ "text": "FIRST LIGHT", "start_s": 0.3, "end_s": 1.8 }] },
    { "job_id": "<video job>", "in_s": 1.0, "out_s": 5.0, "gain_db": -6,
      "captions": [{ "text": "the forge wakes", "start_s": 0.5, "end_s": 3.0 }] },
    { "job_id": "<video job>", "speed": 1.5 },
    { "kind": "still", "job_id": "<image job>", "dur_s": 3, "zoom_to": 1.12 },
    { "kind": "card", "dur_s": 2, "card_color": "#B7410E",
      "captions": [{ "text": "APEX", "start_s": 0.2, "end_s": 1.8 }] }
  ],
  "music": { "job_id": "<audio job>", "gain_db": -8, "fade_in_s": 0.5, "fade_out_s": 1.0 },
  "style": { "letterbox_frac": 0.12, "letterbox_reveal_s": 1.5, "loudnorm": true },
  "video_fade_in_s": 0.3, "video_fade_out_s": 0.75,
  "audio_fade_in_s": 0.3, "audio_fade_out_s": 1.0
}
```

Rules the engine enforces (honest errors, never silent):

- Caption times are **segment-local** seconds; an over-long window is fine
  (clamped by the segment). `out_s: 0` = the clip's full remaining length.
- `speed` 0.5–2.0, clips only. `zoom_to` 1.0–3.0, stills only (1.12 = gentle
  push; 1.25 = assertive). Stills and cards **require** `dur_s`.
- Colors: names or `#RRGGBB`. One music bed; it must resolve to audio.
- Cards take no `job_id` — their captions ARE the content.

## Craft discipline (what made the VIMANA trailer work)

- **Cut on motion, trim ruthlessly.** Generated clips front-load their best
  seconds — `in_s`/`out_s` around the good part beats using all of it.
- **Cards are structure.** Open with one, close with one; mid-cards separate
  acts. Dark backgrounds (`#101418`), short text, ~2s.
- **Captions carry the story** so the piece works muted. One idea per caption.
- **Music under everything** at −6 to −10 dB, faded out over the last second.
- **Letterbox + reveal** for anything that should feel like a trailer;
  `loudnorm: true` on the version you'd call finished.
- **Iterate cheaply.** The segment cache means changing one card re-encodes
  one card. Render early, look (ask the human to ▶, or fetch the thumb),
  adjust, re-render.

## Scoring — where Sonus meets the cut

Composing happens with your **sonus tools**; the cut needs the track **inside
the Imaginarium library** as an audio job. Two honest paths today:

1. The human hits **🎵 SCORE** in the Imagine window's CUT mode and picks your
   track — it lands as a library audio job; `imaginarium_jobs_list` then shows
   it (a `craft_export` job whose asset kind is audio). Reference its id in
   `music`.
2. A track already imported (any earlier SCORE) is reusable forever — check
   the inventory before asking for the import.

There is no MCP audio-import tool yet (big WAVs don't fit the 16 MB frame
cap); when you compose something new, say so and ask for one SCORE click.

## Make it yours

This file is the seed, not the leash. If you cut on this node regularly:

- `store_procedure` the loop above with your own refinements — your trim
  instincts, your card palette, what worked.
- The visual-artist identity (apex-3's Evolution 16) can grow a style layer
  for this: propose it through your own evolution channel
  (`propose_evolution`) — palettes, pacing, a signature card. The charter's
  A7 intent is exactly this: the skill living in *your* memory, not in docs.
