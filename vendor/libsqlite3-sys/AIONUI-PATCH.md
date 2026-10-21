# AionCore SQLite compatibility patch

This is the published `libsqlite3-sys` 0.30.1 crate, retained to satisfy the
existing SQLx 0.8.6 and rusqlite 0.32.1 dependency constraints. Its SQLite
amalgamation (`sqlite3.c`, `sqlite3.h`, `sqlite3ext.h`) is replaced with the
unmodified official SQLite 3.51.3 release. Rust API signatures and build
options are unchanged; the generated bindings' three version constants are
updated to match the compiled source. SQLCipher is unchanged and is not used
by AionCore's bundled SQLite build.

Provenance:

- Crate: https://crates.io/api/v1/crates/libsqlite3-sys/0.30.1/download
  SHA-256: `2e99fb7a497b1e3339bc746195567ed8d3e24945ecd636e3619d20b9de9e9149`
- SQLite: https://sqlite.org/2026/sqlite-amalgamation-3510300.zip
  SHA-256: `acb1e6f5d832484bf6d32b681e858c38add8b2acdfd42ac5df24b8afb46552b4`
- Source ID: `2026-03-13 10:38:09 737ae4a34738ffa0c3ff7f9bb18df914dd1cad163f28fd6b6e114a344fe6d618`
- Advisory: https://sqlite.org/wal.html#walresetbug
- Release: https://sqlite.org/releaselog/3_51_3.html
- The release page's published SHA3-256 for `sqlite3.c` was independently
  verified: `32d5424f97e0a7fc5ed2f6335afbb58be4e0298bd7117a34e39d345ff13d859e`.

This compatibility patch avoids the wider SQLx 0.9 API migration in a
maintenance fix. Replace it with a compatible upstream dependency release
when upgrading SQLx/rusqlite together. The file-backed persistence tests
exercise actual SQLx queries, migrations, WAL snapshots and rusqlite backup;
the runtime-version test rejects an accidentally linked vulnerable library.

SQLite remains public domain. The Rust wrapper retains its upstream MIT
license. No source amalgamation edits or custom SQLite behavior are added.
