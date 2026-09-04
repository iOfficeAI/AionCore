---
name: workmate-presentation
description: "Create or revise a WorkMate native semantic presentation source (*.workmate-deck.json). Use for new decks and slides that should open in the conversation presentation studio and export to editable PPTX. Do not use for arbitrary existing PPTX files, which remain read-only and use officecli-pptx."
---

# WorkMate Native Presentation

Create a compact `*.workmate-deck.json`; never emit OOXML, HTML/CSS, or hundreds of low-level shape commands. Read `references/deckspec-v1.md` before authoring and use `references/example.workmate-deck.json` as the structural example.

## Two-stage workflow

1. Extract the goal, audience, evidence, key conclusion, language, and suggested page count.
2. Ask which catalog theme to use (or propose one) before filling slide content. Prefer brand themes `csbu-workmate` / `csbu-workmate-night` when the deck should match CSBU WorkMate UI identity. Record the choice on `theme.id`. When offering choices, describe CSBU WorkMate theme preview strips (Studio token bands / `references/theme-strips/*.svg`) — never third-party theme grids. Use `officecli deck theme-remap` to change themes after fill and review layout remap suggestions.
3. Write `stage: "outline"` with stable slide IDs, page roles, titles, theme choice, and empty semantic blocks only where useful. For long decks (about 12+ pages, or when the user asks for a sectioned narrative), prefer `officecli deck scaffold` (OfficeCLI ≥ 1.0.162) — or the equivalent long-deck outline heuristics below — instead of inventing a flat bullet list of pages.
4. Ask the user to confirm the outline in Presentation Studio (goal, audience, theme, and slide titles). Do not silently advance this file to `ready`, and do not fill layouts/blocks until that confirmation.
5. After the user confirms (studio sets `stage: "ready"`), fill layouts, typed blocks, speaker notes, and asset requirements.
6. Right after outline confirmation — and again after the first full fill — proactively offer Studio polish (see dialogue cases below). Do not block filling content on these suggestions.
7. Run `officecli deck validate <spec> --json`. Resolve every error before export.
8. Let WorkMate/AionCore build the PPTX; do not call low-level PPTX shape operations. Optional PDF: `officecli deck export-pdf` (exporter plugin / LibreOffice — same as `view <pptx> pdf`, never HTML/Chrome).

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

## Post-outline / post-fill proactive dialogue (CSBU WorkMate)

Offer short, concrete next steps in the user’s language. Each case is original WorkMate wording — adapt names/ids to the open deck.

1. **Layout switch via layout-query** (after outline confirm, before or during fill)  
   “大纲已确认。我可以按内容量跑 `officecli deck layout-query`，给 KPI 页在 `metrics` / `metrics-row-4` / `chart-with-kpis` 里挑更贴合的版式，并把 top-k 写入 `slides[].candidates`，你在 Studio 芯片上一键切换；导出仍只看当前 `layoutId`。”

2. **Media fill for pending assets** (right after first fill when any asset is `pending`)  
   “正文已填好，但还有几张图是 pending。你要我现在按页生成配图并落到 `.assets/`，还是你在 Studio 里用「上传 / 工作区 / 生成图」自己补？跳过未用图也不会挡住导出。”

3. **Theme remap with WorkMate strips** (outline stage or after fill)  
   “主题还可以换皮：优先 CSBU 品牌主题 `csbu-workmate` / `csbu-workmate-night`（WorkMate UI token，不是 Dashi 克隆）。Studio 大纲区有自产样张条；也可用 `officecli deck theme-remap <spec> --to csbu-workmate --json`（`--apply` 写入 `theme.id` + `extensions.themeRemap`，并标出可能需换版式的页与 layout-query 同角色备选）。版式与正文槽位默认保留。要我按听众正式程度帮你挑一个吗？”

4. **Slot visibility toggles** (after fill on layouts with toggleable slots)  
   “这一页的 insight / 模块槽在 Studio 里可以按 `slot.<id>.visible` 显隐，预览和导出一致。若某一栏暂时没证据，我可以帮你关掉对应槽并重排 moduleCount，而不是删掉整页。”

5. **Chart type control** (trend / metrics pages with chart blocks)  
   “趋势页现在是柱状图。如果你更想看占比或漏斗，我可以在不改数据的前提下把 `controls.chartType` 调成 `line` / `doughnut` / `funnel`（以 catalog 控件选项为准），你确认后我再 validate。”

6. **Candidates pin polish** (optional follow-up when user likes two layouts)  
   “这两个同角色版式你都觉得可用的话，我可以把备选 id 留在 `candidates[]`，Studio 芯片会优先展示；点芯片会走换布局并尽量保留 blocks，导出不会把候选写成第二套正文。”

7. **Long-deck scaffold** (when user asks for 12+ pages / 长稿 / sectioned narrative)  
   “页数比较多的话，我可以用 `officecli deck scaffold` 先搭大纲：封面、议程、按目标/听众偏向的角色混排，并在长稿里插入 `transition` 分节；同角色版式会预置到 `candidates[]`。种子参数可复现。你确认 goal / audience / 页数后我就生成 outline 给你在 Studio 里改标题。”

8. **Wireframe candidate compare** (when several same-role layouts are plausible)  
   “同角色有好几个版式时，可以在 Studio 检查器打开「对照版式」：并排或翻页看槽位线框（不是 HTML 截图），点线框就切换 layout 并尽量保留 blocks。需要的话我先把 top-k 写入 `candidates[]`。”



9. **PDF export (optional)**  
   “如果要 PDF，可用 `officecli deck export-pdf <spec> -o out.pdf --json`（OfficeCLI ≥ 1.0.163）：先 `deck build` 出 PPTX，再走与 `officecli view <pptx> pdf` 相同的 exporter 插件（常见包装 LibreOffice）。未安装 exporter 时会返回 `exporter_not_found`；这不是 HTML/Chrome 导出。Studio 默认仍导出可编辑 PPTX。”


## Long-deck scaffold (P2.7)

When page count is high or the narrative needs chapter breaks, scaffold an outline first — original WorkMate heuristics, not a third-party goal-spec clone:

1. Confirm goal, audience, language, theme, and target page count (4–60). Prefer theme `csbu-workmate` / `csbu-workmate-night` for CSBU identity.
2. Run:
   `officecli deck scaffold --goal "…" --audience "…" --pages N --theme csbu-workmate --seed <stable> -o outline.workmate-deck.json --json`
   (If CLI < 1.0.162, hand-author the same structure: cover → optional agenda → content roles → section `transition` every ~5–7 content slides → actions/closing.)
3. Keep `stage: "outline"`; titles may be placeholders. Same-role `candidates[]` may already be pinned — leave them for Studio chips / wireframe compare.
4. Ask the user to confirm the outline (titles + theme) in Presentation Studio before filling blocks.
5. Re-run with the same `--seed` when the user only tweaks wording but wants the same role spine.

Role-mix bias (keyword hints in goal/audience): metrics / trend / comparison / process / risks / team / case / actions. Always bookend with `cover` and `closing`.

## Safety and quality

- Use only catalog theme, layout, slot, and control IDs returned by `officecli deck catalog --json`.
- Keep one idea per slide and add speaker notes to every content slide.
- Use charts and tables only for real structured data. Never encode them as screenshots.
- Keep all asset paths relative and inside the deck directory. Never download remote URLs during compilation.
- Preserve `schemaVersion`; increment `revision` for each saved semantic edit.
- Branding stays CSBU WorkMate; do not vendor AGPL decks or third-party theme preview assets.
