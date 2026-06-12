# Vendored Slint reference (std-widgets + element fundamentals)

Authoritative widget/element API, copied verbatim from the official Slint repo's
Starlight docs. This is the **anti-hallucination reference** — exact property
names, types, defaults, and visibility — to consult instead of guessing Slint
syntax from memory.

> Provenance: `slint-ui/slint` @ commit in `.upstream-sha`
> (`docs/astro/src/content/docs/reference/`). Not the dead `slint.dev` site,
> not the AI-generated `lib-slint-expert` skill (deleted — that one was riddled
> with invented syntax like `elevation: 2dp` and `@theme := …`).

## What's here

- `std-widgets/` — every standard widget: `basic-widgets/`, `views/`,
  `layouts/`, `globals/` (Palette, StyleMetrics), `misc/`, plus `overview.mdx`
  and `style.mdx`.
- `common.mdx` — properties shared by all elements (geometry, visibility, etc.).
- `primitive-types.mdx` — the type system (`length`, `duration`, `brush`, …).
- `colors-and-brushes.mdx` — color/brush literals and functions.
- `layouts/overview.mdx` — layout model.

The `.mdx` files keep their Starlight frontmatter and `<SlintProperty .../>` /
`<Link .../>` JSX tags. Those imports don't resolve outside the Slint repo, but
the tags carry the structured data (propName/typeName/defaultValue) and read
fine as-is.

## NOT vendored — fetch on demand from the repo

Lower-frequency reference; pull a specific file when a subsystem needs it rather
than mirroring the whole tree (avoids bloat + staleness):

- `guide/language/` — full language guide (concepts, syntax, bindings)
- `reference/keyboard-input/`, `global-functions/`, `global-namespaces/`
- Built-in element pages (Rectangle/Text/Image/TouchArea/Path) — generated, not
  MDX; see `guide/language/` and `common.mdx`
- `AGENTS.md` + `docs/development/*` — compiler internals, only if hacking Slint
  itself (we're consumers, not contributors)

## Refresh

```bash
# from repo root — re-pull at the current upstream master
SHA=$(gh api repos/slint-ui/slint/commits/master --jq .sha)
BASE="https://raw.githubusercontent.com/slint-ui/slint/$SHA/docs/astro/src/content/docs/reference"
FILES=$(gh api "repos/slint-ui/slint/git/trees/$SHA?recursive=1" \
  --jq '.tree[].path | select(startswith("docs/astro/src/content/docs/reference/std-widgets/") and endswith(".mdx"))')
FILES="$FILES
docs/astro/src/content/docs/reference/common.mdx
docs/astro/src/content/docs/reference/primitive-types.mdx
docs/astro/src/content/docs/reference/colors-and-brushes.mdx
docs/astro/src/content/docs/reference/layouts/overview.mdx"
while IFS= read -r p; do [ -z "$p" ] && continue
  rel="${p#docs/astro/src/content/docs/reference/}"
  mkdir -p "docs/slint-reference/$(dirname "$rel")"
  curl -sf "$BASE/$rel" -o "docs/slint-reference/$rel"
done <<< "$FILES"
echo "$SHA" > docs/slint-reference/.upstream-sha
```
