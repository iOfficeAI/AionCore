---
name: workmate-presentation
description: "Create or revise a WorkMate native semantic presentation source (*.workmate-deck.json). Use for new decks and slides that should open in the conversation presentation studio and export to editable PPTX. Do not use for arbitrary existing PPTX files, which remain read-only and use officecli-pptx."
---

# WorkMate Native Presentation

Create a compact `*.workmate-deck.json`; never emit OOXML, HTML/CSS, or hundreds of low-level shape commands. Read `references/deckspec-v1.md` before authoring and use `references/example.workmate-deck.json` as the structural example.

## Two-stage workflow

1. Extract the goal, audience, evidence, key conclusion, language, and suggested page count.
2. Ask which catalog theme to use (or propose one) before filling slide content. Record the choice on `theme.id`.
3. Write `stage: "outline"` with stable slide IDs, page roles, titles, theme choice, and empty semantic blocks only where useful.
4. Ask the user to confirm the outline in Presentation Studio (goal, audience, theme, and slide titles). Do not silently advance this file to `ready`, and do not fill layouts/blocks until that confirmation.
5. After the user confirms (studio sets `stage: "ready"`), fill layouts, typed blocks, speaker notes, and asset requirements.
6. Right after outline confirmation, briefly suggest Studio polish the user can do themselves: switch same-role layouts, adjust moduleCount/balance/mediaSide controls, fill pending images (upload/generate), or change the deck theme. Do not block filling content on these suggestions.
7. Run `officecli deck validate <spec> --json`. Resolve every error before export.
8. Let WorkMate/AionCore build the PPTX; do not call low-level PPTX shape operations.

## Media

- Represent an unfilled visual as an image block plus a declared asset with `status: "pending"`.
- Generate images only when the user asks or clicks the studio action.
- Use the configured WorkMate image-generation tool, save the result below the sibling `<deck-name>.assets/` directory, then set the asset to `ready`.
- Record useful alt text, source, model, and a short prompt summary. Never store credentials or a full sensitive prompt.
- On failure, set the pending asset to `error`; never delete or overwrite a previous ready asset.

## Layout and role selection

- Prefer `officecli deck layout-query --json` (OfficeCLI ≥ 1.0.159) to rank layouts by `--role`, `--item-count` / `--module-count`, `--has-chart`, `--needs-media` / `--has-image`, and optional `--query`. Fall back to `officecli deck catalog --json` and Studio same-role chips only when layout-query is unavailable.
- Match page intent to a semantic role first, then pick a layout whose slots fit the content volume:
  - Narrative cover / TOC / section: `cover`, `breakdown`, `transition`
  - Evidence: `metrics`, `trend` (incl. `chart-waterfall` / `chart-funnel` when useful), `distribution`
  - Analysis: `comparison` packs such as `swot`, `pest`, `five-forces`, `bmc-lite`, `raci`, `double-diamond`
  - Risk / next steps: `risks`, `actions`, `result`, `closing`
  - People / story: `team`, `case`, `relationship`, `context`, `observation`
- When several layouts share a role, prefer the one whose module capacity is closest to the number of items/KPIs, and that accepts chart/image blocks when the slide needs them.
- Optional: set `slides[].candidates` to the top-k layout ids from layout-query (Studio chips prefer these). Export still uses `layoutId` only; validate rejects unknown candidate ids.

## Safety and quality

- Use only catalog theme, layout, slot, and control IDs returned by `officecli deck catalog --json`.
- Keep one idea per slide and add speaker notes to every content slide.
- Use charts and tables only for real structured data. Never encode them as screenshots.
- Keep all asset paths relative and inside the deck directory. Never download remote URLs during compilation.
- Preserve `schemaVersion`; increment `revision` for each saved semantic edit.
