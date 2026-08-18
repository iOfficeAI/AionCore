# 2dev Team migration repair 使用说明

`repair-2dev-team-migrations.sh` 用于处理历史二开 Team migrations 与新版 upstream migration 编号冲突。

## 使用原则

- 默认是 dry-run；不带 `--apply` 时禁止修改数据库。
- 自动 repair 只接受脚本内白名单明确识别的历史窗口。
- 识别条件同时包含 description、SHA-384 checksum、`success=1`、checksum BLOB 类型与 48 字节长度。
- 禁止只根据 migration version 猜测或改写。
- `--apply` 会在任何 mutation 前使用 SQLite `.backup` 创建完整备份。
- metadata remap 在 `BEGIN IMMEDIATE` 事务中完成并执行 post-check。
- repair 只修复 `_sqlx_migrations` metadata，不重复执行已经执行过的历史 migration SQL。
- repair 完成后必须启动正常 AionCore migrator，让新的 pending migrations 正常执行。

## v0.1.67 已验证窗口

### redo

历史生产库：

```text
038 ad hoc team origin conversation
039 team presets
040 backfill formal team leader team id
```

目标：

```text
039 ad hoc team origin conversation
040 team presets
041 backfill formal team leader team id
```

映射：

```text
038 -> 039
039 -> 040
040 -> 041
```

随后正常 migrator 执行：

```text
042 remove orphaned team conversation bindings
```

### legacy

脚本同时保留对更老历史窗口 034/035/036 -> 039/040/041 的白名单支持，并由自动化测试覆盖。

## 生产升级推荐流程

先对真实生产数据库的离线副本执行：

```bash
bash scripts/migration/repair-2dev-team-migrations.sh /path/to/aionui-backend.db
```

如果输出 `Detected source window`，核对计划映射后，生产切换阶段才执行：

```bash
bash scripts/migration/repair-2dev-team-migrations.sh --apply /path/to/aionui-backend.db
```

成功后保留自动生成的 `.bak`，启动新版 AionCore，再读取 `_sqlx_migrations` 确认最终 migration 全部 `success=1`。

## macOS SQLite CLI 兼容性

2026-08-18 的真实生产数据库验证发现：

```bash
sqlite3 -readonly DATABASE ...
```

会返回 SQLite error 14 (`unable to open database file`)，但普通连接执行 SELECT 正常。

因此脚本的只读 preflight 使用普通 sqlite3 connection，并在查询中先执行：

```sql
PRAGMA query_only=ON;
```

`query_only` 会阻止该连接执行写操作，同时避免依赖 CLI `-readonly` 打开模式。不要把脚本重新改回 `sqlite3 -readonly`，除非重新验证目标 macOS/SQLite/生产数据库组合。

## 自动化门禁

修改 repair 脚本后至少运行：

```bash
bash scripts/migration/repair-2dev-team-migrations.test.sh
```

必须覆盖：

- fresh/current no-op；
- redo remap；
- second-run idempotency；
- legacy remap；
- checksum mismatch reject；
- foreign target reject；
- source/target collision reject。
