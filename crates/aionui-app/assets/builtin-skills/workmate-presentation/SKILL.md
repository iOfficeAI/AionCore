---
name: workmate-presentation
description: "Create or revise a WorkMate native semantic presentation source (*.workmate-deck.json). Use for new decks and slides that should open in the conversation presentation studio and export to editable PPTX. Do not use for arbitrary existing PPTX files, which remain read-only and use officecli-pptx."
---

# WorkMate Native Presentation

Create a compact `*.workmate-deck.json`; never emit OOXML, HTML/CSS, or hundreds of low-level shape commands. Read `references/deckspec-v1.md` before authoring and use `references/example.workmate-deck.json` as the structural example.

## Two-stage workflow

1. Extract the goal, audience, evidence, key conclusion, language, and suggested page count.
2. Write `stage: "outline"` with stable slide IDs, page roles, titles, theme choice, and empty semantic blocks only where useful.
3. Ask the user to confirm the outline and theme. Do not silently advance this file to `ready`.
4. After confirmation, fill layouts, typed blocks, speaker notes, and asset requirements; set `stage: "ready"`.
5. Run `officecli deck validate <spec> --json`. Resolve every error before export.
6. Let WorkMate/AionCore build the PPTX; do not call low-level PPTX shape operations.

## Media

- Represent an unfilled visual as an image block plus a declared asset with `status: "pending"`.
- Generate images only when the user asks or clicks the studio action.
- Use the configured WorkMate image-generation tool, save the result below the sibling `<deck-name>.assets/` directory, then set the asset to `ready`.
- Record useful alt text, source, model, and a short prompt summary. Never store credentials or a full sensitive prompt.
- On failure, set the pending asset to `error`; never delete or overwrite a previous ready asset.

## Safety and quality

- Use only catalog theme, layout, slot, and control IDs returned by `officecli deck catalog --json`.
- Keep one idea per slide and add speaker notes to every content slide.
- Use charts and tables only for real structured data. Never encode them as screenshots.
- Keep all asset paths relative and inside the deck directory. Never download remote URLs during compilation.
- Preserve `schemaVersion`; increment `revision` for each saved semantic edit.
