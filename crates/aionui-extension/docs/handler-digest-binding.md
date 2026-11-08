# Optional agent-tool handler digest binding

## What this adds

`ExtAgentTool` may include optional `handlerIntegrity` (serde field
`handler_integrity`, JSON alias `handlerIntegrity`). The value, when present,
must be the **canonical** SHA-512 SRI string used by the experimental source
checker: exactly 95 bytes, lowercase `sha512-`, standard Base64 alphabet,
`==` padding, and zero padding bits in the final data sextet (`A`/`Q`/`g`/`w`).

Shared syntax helper: `hub::is_canonical_sha512_sri`.

## What absence means

**Missing/None does not mean verified.** Back-compat manifests without the field
continue to parse. Absence must not be treated as a skip/allow for integrity.

## Pure helpers

- `expected_handler_integrity(&ExtensionManifest, tool_id)` looks up by **exact
  tool id** only. Duplicate ids in `agent_tools` fail closed (`AmbiguousToolId`)
  because `serde_json` keeps every array element. Duplicate object keys inside
  one tool object follow Value last-wins before typing; that is documented and
  tested, not a silent multi-digest merge.
- `bind_handler_digest_from_signed(&SignatureCheckedManifest, tool_id)` re-parses
  the owned signed **byte snapshot** only (no filesystem reread) and returns
  `(handler path, expected SRI)` when the field is present and valid.

Present invalid SRI, or present SRI without a non-empty `handler`, is rejected at
`parse_manifest` / `validate_manifest` (fail closed). Helpers also return fixed
non-sensitive error categories and never invent a digest.

## Trust limits

A present field inside signed manifest bytes can declare which script SRI the
author/index intended **for that tool id**. It becomes a binding only when a host
also:

1. trusts the signed manifest snapshot (`check_signed_manifest`),
2. resolves a trustworthy root / `/proc` view,
3. reads the handler once (`read_bounded_source` / `read_checked_source`) and
   compares against the extracted expected SRI.

This document/PR alone does **not** complete a signed-manifest→handler chain,
authorize execution, prove curation/owner/root, or change installer/routes/gates.
No handler file IO is performed by these helpers. Not M9/E13 completion.

## Host composition (app)

`aionui_app::extension_js_handler_source::load_checked_handler_source_from_signed`
composes this binder with `read_checked_source` against a Host-supplied trusted
root. Missing integrity remains `NotPresent` (no unchecked fallback). Success
returns the same owned verified source `String`. Still not execution auth,
curation/owner/root proof, or M9/E13 completion.


## Host invoke prep (app) + route refuse

`aionui_app::extension_js_invoke_checked::prepare_checked_agent_tool_invoke`
wraps the composition into a fail-closed invoke decision: Ok → non-executing
ack tokens (`handler_integrity_checked`, `dispatched=false`) with Host-private
source; Err → fixed refuse reasons (`handler_integrity_not_present`, …). Never
`handler_stub` success on bind/check failure.

HTTP `POST /api/extensions/agent-tools/invoke` cannot call that helper yet:
`ExtensionRouterState` has no Host-held `SignatureCheckedManifest` or trusted
root, and `aionui-extension` must not depend on `aionui-app`. The route therefore
refuses with `integrity_binding_unavailable` after the existing planSafeActions
gate rather than inventing trust or claiming stub dispatch. Not M9/E13 complete.
