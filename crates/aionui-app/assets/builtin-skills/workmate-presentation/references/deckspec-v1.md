# DeckSpec v1 authoring contract

Required root fields are `schemaVersion: 1`, a non-negative `revision`, `stage`, `metadata`, `theme`, `slides`, and `assets`.

- Metadata requires `title`, `language`, and `aspectRatio: "16:9"`.
- Every slide has a stable unique `id`, semantic `role`, catalog `layoutId`, `blocks`, optional `notes`, optional `hidden`, optional catalog controls, and optional `candidates` (preferred alternate layout ids from `officecli deck layout-query`; export uses `layoutId` only).
- Every block has a stable unique `id`, one supported `type`, and normally an explicit catalog `slot`.
- Supported block types are `text`, `list`, `metric`, `image`, `chart`, `table`, `timeline`, `quote`, and `shape`.
- Chart data uses `{ "chartType": "column", "categories": ["Q1"], "series": [{ "name": "Revenue", "values": [42] }] }`.
- Table data uses `{ "rows": [["Metric", "Value"], ["Revenue", "42"]] }`.
- Assets use a relative `path`, type `image`, status `pending`, `ready`, or `error`, and useful `alt` text.

Limits: 100 slides, 64 blocks per slide, 2 MB source, 25 MB per asset, and 200 MB total assets. `ready` decks cannot export while required slots or assets are unresolved.

Theme previews for outline picking are CSBU WorkMate token strips under `references/theme-strips/` (and Studio programmatic bands). Do not embed third-party theme grids.
