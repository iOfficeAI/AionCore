# Strict signed manifest byte-snapshot check

`hub::signed_manifest::check_signed_manifest` is an independent pure primitive,
not an execution permit, install receipt, curation assertion or registry gate.
It is not wired to services, HTTP, installation, activation or the JS reader.
Existing install semantics and the private `HubIndexEntry` type are unchanged.

## Explicit inputs and trust boundary

The API accepts a public borrowed `SignedManifestEntry` projection (name, version,
optional key id, integrity and hex signature), a bounded manifest byte slice,
and the existing public `CuratedInstallPolicy` passed explicitly by the host.
The **host must acquire a trusted key map/policy**. Keys obtained from this
manifest, hub index or user request do not establish trust. No environment or
global policy is loaded by the primitive. A nonempty explicit allowlist is
respected. `require_signature` is deliberately irrelevant: a known trusted key,
nonempty key id, integrity and valid signature are required on every call.
The projection has no bundled, curated or optional-signature bypass flags.
A configured trusted key is not itself proof of any curation process or owner.

## Exact bytes, bounded metadata, signature format

- Original raw manifest: at most **262144 bytes (256 KiB)**, checked before hashing
  or parsing. This is a bytes/allocation input bound, not a wall-clock guarantee.
- Name at most 256 UTF-8 bytes, version at most 128 bytes; both nonempty, without
  Unicode control characters (including LF/CR), checked before constructing the
  existing canonical `name\nversion\nintegrity` payload.
- Key id: 1..128 ASCII alphanumeric / dot / underscore / hyphen bytes, and must
  select an actual key in the host's explicit map. The host-owned map/allowlist
  size is not bounded here; unrelated index fields are not accepted or copied.
- Integrity: exactly 95 bytes and **exactly equal**, case-sensitively, to existing
  `sri_sha512(raw)`: lowercase `sha512-` plus canonical padded base64 SHA-512.
  No algorithm aliases, alternate padding, whitespace or digest-case folding.
- Signature: exactly **128 hex characters decoding to 64 bytes**, consistent
  with the existing hub signature encoding. A fixed array is used for decoding;
  no base64 fallback, trimming or unbounded signature allocation.
- Existing `signature_payload` is verified with Ed25519 `verify_strict` using
  the selected host key. Neither bundled nor an empty configured map can skip it.

The digest always covers the **original raw bytes**, never normalized JSON or a
reserialized manifest. The existing pure `parse_manifest(raw)` then parses the
same bytes, normalizes existing legacy fields, and validates existing
name/reserved-prefix/semver rules. It does not resolve `$file:` references or
read files (the directory-aware parser is not called). Parsed name and version
must exactly equal the signed entry's identities. Current legacy normalization
does not rewrite those identities. This is the existing parser contract, not a
new complete schema/capability/contribution policy or duplicate-JSON-key policy.

Success returns `SignatureCheckedManifest`, which privately owns an exact copy
of the checked original bytes and parsed identity/key-id strings. There is no
unchecked constructor, Default, Deserialize or raw-data Debug formatter. Getters
borrow those owned values. **Future consumers must use this byte snapshot,
not reread a file after checking**, and must not infer execution authority from
this type. Legacy normalization may change parsed fields but never the stored
bytes. `$file:` references, handlers or other referenced content are not bound by
this snapshot. Error categories never contain manifest/environment/key values.

## Limits and tests

No root path, filesystem owner, handler byte identity, installation receipt,
curation workflow, key revocation/freshness or disk-index immutability is proven.
Hardlinks, in-place writes, native engine risks, runtime shutdown, OS sandboxing,
HTTP/user/plan-safe authorization and execution integration remain separate.
No new manifest fields or handler-digest/receipt format is introduced. This is
not M9/E13 completion.

Deterministic tests use explicit public test SigningKey fixtures and policy maps,
never real credentials, environment mutation or global privileges. They cover
exact owned bytes/identity (including legacy normalization and literal `$file`),
missing/unknown keys and required metadata despite `require_signature=false`,
explicit allowlist behavior, wrong keys and tampering, exact digest case,
signed/parsed identity mismatch, malformed JSON/existing validation errors,
raw and metadata byte boundaries/control characters, malformed hex and no base64
fallback. A known identity-point key/invalid signature fixture checks strict
rejection only; it makes no crypto-exploitation claim. There is no bundled or
curated input flag to exercise as a bypass.

```sh
cargo +1.95.0 test -p aionui-extension --lib hub::signed_manifest::
cargo +1.95.0 test -p aionui-extension --lib
cargo +1.95.0 clippy -p aionui-extension --lib -- -D warnings
cargo +1.95.0 clippy --workspace -- -D warnings
cargo +1.95.0 fmt --all -- --check
git diff --check
```
