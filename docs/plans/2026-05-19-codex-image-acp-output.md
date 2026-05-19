# Codex 图片 ACP 输出修复实施计划

**目标：** 修复 Codex 自带图片生成通过 ACP 接入 AionUi 时会话停在执行中、数据库和前端承载大段图片 base64 的问题，并让生成图片可直接预览。

**架构：** 在 AionCLI 的 ACP 翻译边界清洗 Codex 图片工具输出，确保 WebSocket 和数据库都只接收小型结构化结果。AionUi 只负责识别清洗后的 `saved_path` / `image.path` 并复用已有本地图片读取能力展示预览。

**技术栈：** Rust / Tokio / serde_json / ACP SDK event mapping / React / TypeScript / Arco Design / bun / cargo nextest 或 cargo test。

---

## 背景事实

当前数据流：

1. Codex ACP 产出 `SessionUpdate::ToolCallUpdate`。
2. `AionCLI/crates/aionui-ai-agent/src/protocol/events/translate.rs` 直接复制 `tcu.fields.raw_output`。
3. `AionCLI/crates/aionui-conversation/src/stream_relay.rs` 先把事件转发到 WebSocket，再持久化到 `messages.content`。
4. AionUi 前端收到 `acp_tool_call` 后直接合并进消息列表并渲染工具卡。

实测异常样本：

- 会话：`a747f520`
- 消息：`ig_0597c8b499d4f9bd016a0b03149e50819bb807f73f080e2822`
- DB 单条 `messages.content` 约 `2.8MB`
- `raw_output.result` 是完整 PNG base64
- `raw_output.saved_path` 指向已生成文件
- 工具状态仍为 `in_progress/generating`，导致前端看起来卡在执行中

## 设计原则

- 大字段必须在 AionCLI ACP 边界被移除，不能等到前端处理。
- 普通 shell/read/edit 工具输出不能被误删。
- 图片已落盘时，以文件路径作为唯一大图载体。
- 前端展示只消费小型结构化字段，不解析完整 base64。
- 测试先行，先复现大 base64 和状态卡住，再实现最小修复。

---

### 任务 1：后端单测覆盖 Codex 图片 raw_output 清洗

**文件：**

- 修改：`AionCLI/crates/aionui-ai-agent/src/protocol/events/translate.rs`
- 测试：`AionCLI/crates/aionui-ai-agent/src/protocol/events/mod.rs`

**步骤 1：写失败测试**

在 `events/mod.rs` 的现有 ACP tool call 测试附近新增测试：

```rust
#[test]
fn codex_image_tool_update_omits_base64_result() {
    use agent_client_protocol as acp;
    use serde_json::json;

    let large_png_base64 = format!("iVBORw0KGgo{}", "A".repeat(128 * 1024));
    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::from("ig_test_image"),
        acp::ToolCallUpdateFields::new()
            .status(acp::ToolCallStatus::InProgress)
            .raw_output(json!({
                "call_id": "ig_test_image",
                "status": "generating",
                "saved_path": "/Users/test/.codex/generated_images/session/ig_test_image.png",
                "revised_prompt": "一只小猫",
                "result": large_png_base64
            })),
    ));

    let events = translate_session_update("session-1".to_owned(), update);
    let json = serde_json::to_value(&events[0]).unwrap();
    let raw_output = &json["data"]["update"]["raw_output"];

    assert_eq!(raw_output["saved_path"], "/Users/test/.codex/generated_images/session/ig_test_image.png");
    assert_eq!(raw_output["image"]["path"], "/Users/test/.codex/generated_images/session/ig_test_image.png");
    assert_eq!(raw_output["result_omitted"], true);
    assert!(raw_output.get("result").is_none());
}
```

**步骤 2：运行测试确认失败**

运行：

```bash
cd AionCLI
cargo test -p aionui-ai-agent codex_image_tool_update_omits_base64_result
```

预期：FAIL，原因是当前 `raw_output.result` 仍保留完整 base64，且没有 `image` / `result_omitted` 字段。

**步骤 3：实现最小清洗函数**

在 `translate.rs` 增加私有函数：

```rust
const ACP_RAW_OUTPUT_INLINE_IMAGE_LIMIT: usize = 64 * 1024;

fn sanitize_raw_output(raw_output: Option<serde_json::Value>) -> Option<serde_json::Value> {
    let mut value = raw_output?;
    sanitize_inline_image_result(&mut value);
    Some(value)
}

fn sanitize_inline_image_result(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    let saved_path = obj
        .get("saved_path")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let should_omit = obj
        .get("result")
        .and_then(|v| v.as_str())
        .map(|s| saved_path.is_some() && is_probably_inline_image_result(s))
        .unwrap_or(false);

    if !should_omit {
        return;
    }

    let result_len = obj.get("result").and_then(|v| v.as_str()).map(str::len).unwrap_or(0);
    obj.remove("result");
    obj.insert("result_omitted".to_owned(), serde_json::Value::Bool(true));
    obj.insert(
        "result_omitted_reason".to_owned(),
        serde_json::Value::String("image_base64".to_owned()),
    );
    obj.insert(
        "result_bytes".to_owned(),
        serde_json::Value::Number(serde_json::Number::from(result_len)),
    );

    if let Some(path) = saved_path {
        obj.insert(
            "image".to_owned(),
            serde_json::json!({
                "path": path,
                "mime_type": mime_type_from_image_path(&path),
                "source": "codex_image_generation"
            }),
        );
    }
}

fn is_probably_inline_image_result(value: &str) -> bool {
    value.len() > ACP_RAW_OUTPUT_INLINE_IMAGE_LIMIT
        && (value.starts_with("iVBORw0KGgo")
            || value.starts_with("/9j/")
            || value.starts_with("UklGR")
            || value.starts_with("data:image/"))
}

fn mime_type_from_image_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else {
        "image/png"
    }
}
```

然后把 `ToolCallUpdate` 分支里的：

```rust
raw_output: tcu.fields.raw_output.clone(),
```

替换为：

```rust
raw_output: sanitize_raw_output(tcu.fields.raw_output.clone()),
```

**步骤 4：运行测试确认通过**

运行：

```bash
cd AionCLI
cargo test -p aionui-ai-agent codex_image_tool_update_omits_base64_result
```

预期：PASS。

**步骤 5：提交**

暂不单独提交，等任务 2 后一起提交后端边界修复。

---

### 任务 2：后端单测覆盖图片工具状态归一化

**文件：**

- 修改：`AionCLI/crates/aionui-ai-agent/src/protocol/events/translate.rs`
- 测试：`AionCLI/crates/aionui-ai-agent/src/protocol/events/mod.rs`

**步骤 1：写失败测试**

新增测试：

```rust
#[test]
fn codex_image_tool_update_with_saved_path_is_completed() {
    use agent_client_protocol as acp;
    use serde_json::json;

    let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::from("ig_done_image"),
        acp::ToolCallUpdateFields::new()
            .status(acp::ToolCallStatus::InProgress)
            .raw_output(json!({
                "call_id": "ig_done_image",
                "status": "generating",
                "saved_path": "/Users/test/.codex/generated_images/session/ig_done_image.png",
                "result": format!("iVBORw0KGgo{}", "A".repeat(128 * 1024))
            })),
    ));

    let events = translate_session_update("session-1".to_owned(), update);
    let json = serde_json::to_value(&events[0]).unwrap();

    assert_eq!(json["data"]["update"]["status"], "completed");
    assert_eq!(json["data"]["update"]["raw_output"]["status"], "completed");
}
```

**步骤 2：运行测试确认失败**

运行：

```bash
cd AionCLI
cargo test -p aionui-ai-agent codex_image_tool_update_with_saved_path_is_completed
```

预期：FAIL，当前状态仍是 `in_progress` / `generating`。

**步骤 3：实现状态归一化**

在 `translate.rs` 增加：

```rust
fn normalize_tool_status(
    sdk_status: Option<&SdkToolCallStatus>,
    raw_output: Option<&serde_json::Value>,
) -> Option<AcpToolCallStatus> {
    if raw_output.and_then(|v| v.get("image")).and_then(|v| v.get("path")).is_some() {
        return Some(AcpToolCallStatus::Completed);
    }

    sdk_status.map(map_sdk_tool_status)
}

fn normalize_raw_output_status(raw_output: &mut Option<serde_json::Value>, status: Option<&AcpToolCallStatus>) {
    let Some(AcpToolCallStatus::Completed) = status else {
        return;
    };
    let Some(obj) = raw_output.as_mut().and_then(|v| v.as_object_mut()) else {
        return;
    };
    obj.insert("status".to_owned(), serde_json::Value::String("completed".to_owned()));
}
```

在 `ToolCallUpdate` 分支先构造：

```rust
let mut raw_output = sanitize_raw_output(tcu.fields.raw_output.clone());
let status = normalize_tool_status(tcu.fields.status.as_ref(), raw_output.as_ref());
normalize_raw_output_status(&mut raw_output, status.as_ref());
```

再填入：

```rust
status,
raw_output,
```

**步骤 4：运行后端相关测试**

运行：

```bash
cd AionCLI
cargo test -p aionui-ai-agent codex_image_tool_update
```

预期：两个新增测试 PASS。

**步骤 5：后端边界提交**

运行：

```bash
cd AionCLI
git add crates/aionui-ai-agent/src/protocol/events/translate.rs crates/aionui-ai-agent/src/protocol/events/mod.rs docs/plans/2026-05-19-codex-image-acp-output.md
git commit -m "fix: sanitize codex image acp output"
```

---

### 任务 3：会话持久化层增加回归测试

**文件：**

- 修改：`AionCLI/crates/aionui-conversation/src/stream_relay.rs`

**步骤 1：写失败测试或补充现有测试**

在现有 `run_acp_tool_call_inserts_then_updates` 附近新增测试，直接发送清洗后的 completed 图片事件：

```rust
#[tokio::test]
async fn run_acp_image_tool_call_update_persists_finish_without_base64() {
    use aionui_ai_agent::protocol::events::tool_call::{
        AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallUpdateData,
    };

    let (relay, tx, repo) = setup_relay_for_test();

    tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
        session_id: "session-1".to_owned(),
        update: AcpToolCallUpdateData {
            session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
            tool_call_id: "ig_test_image".into(),
            status: Some(AcpToolCallStatus::Completed),
            title: Some("Image generation".into()),
            kind: Some(AcpToolCallKind::Execute),
            raw_input: None,
            raw_output: Some(serde_json::json!({
                "saved_path": "/Users/test/.codex/generated_images/session/ig_test_image.png",
                "image": {
                    "path": "/Users/test/.codex/generated_images/session/ig_test_image.png",
                    "mime_type": "image/png",
                    "source": "codex_image_generation"
                },
                "result_omitted": true
            })),
            content: None,
            locations: None,
        },
        meta: None,
    }))
    .unwrap();
    tx.send(AgentStreamEvent::Finish(Default::default())).unwrap();

    relay.run().await.unwrap();

    let msg = repo
        .get_message_by_msg_id("conv-1", "ig_test_image", "acp_tool_call")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(msg.status.as_deref(), Some("finish"));
    assert!(!msg.content.contains("iVBORw0KGgo"));
    assert!(msg.content.contains("result_omitted"));
}
```

如果现有测试 helper 不能直接复用，先按已有 stream relay 测试样式补齐最小 fixture，不引入新测试框架。

**步骤 2：运行测试确认失败或编译失败**

运行：

```bash
cd AionCLI
cargo test -p aionui-conversation acp_image_tool_call
```

预期：初次可能因 fixture 名称不存在失败，按现有测试结构修正。

**步骤 3：最小调整**

如果任务 1/2 已在翻译层处理，这里通常无需业务实现。只需要确保测试使用现有 helper 正确创建 relay 和 mock repo。

**步骤 4：运行测试确认通过**

运行：

```bash
cd AionCLI
cargo test -p aionui-conversation acp_image_tool_call
```

预期：PASS。

**步骤 5：提交**

运行：

```bash
cd AionCLI
git add crates/aionui-conversation/src/stream_relay.rs
git commit -m "test: cover acp image tool persistence"
```

---

### 任务 4：前端类型补齐 ACP raw output 字段

**文件：**

- 修改：`AionUi/packages/desktop/src/common/types/platform/acpTypes.ts`

**步骤 1：写类型结构**

给 `ToolCallUpdate.update` 增加兼容字段：

```ts
export interface AcpImageOutput {
  path: string;
  mime_type?: string;
  source?: string;
}

export interface AcpRawOutput {
  saved_path?: string;
  image?: AcpImageOutput;
  result_omitted?: boolean;
  result_omitted_reason?: string;
  result_bytes?: number;
  status?: string;
  [key: string]: unknown;
}
```

然后在 `ToolCallUpdate.update` 内增加：

```ts
rawOutput?: AcpRawOutput;
raw_output?: AcpRawOutput;
```

保留 `rawInput`，不要破坏现有字段。

**步骤 2：运行类型检查确认现有代码能编译**

运行：

```bash
cd AionUi
bun run typecheck
```

预期：PASS。如果仓库没有 `typecheck` 脚本，改跑 `bun run build`。

**步骤 3：提交**

暂不提交，等任务 5 前端展示一起提交。

---

### 任务 5：前端 ACP 工具卡展示图片预览

**文件：**

- 修改：`AionUi/packages/desktop/src/renderer/pages/conversation/Messages/acp/MessageAcpToolCall.tsx`

**步骤 1：写提取函数**

在组件文件内新增：

```ts
function getAcpImagePath(update: IMessageAcpToolCall['content']['update']): string | undefined {
  const rawOutput = update.rawOutput || update.raw_output;
  const imagePath = rawOutput?.image?.path;
  if (typeof imagePath === 'string' && imagePath) return imagePath;

  const savedPath = rawOutput?.saved_path;
  if (typeof savedPath === 'string' && savedPath) return savedPath;

  return undefined;
}
```

**步骤 2：引入 LocalImageView**

```ts
import LocalImageView from '@/renderer/components/media/LocalImageView';
```

**步骤 3：在工具卡内容区展示图片**

在 `MessageAcpToolCall` 中：

```ts
const imagePath = getAcpImagePath(update);
```

在 `diffContent` 渲染前后加入：

```tsx
{imagePath && (
  <div className='mt-3 overflow-hidden rounded border bg-1 p-2'>
    <LocalImageView
      src={imagePath}
      alt={imagePath.split(/[/\\]/).pop() || 'Generated image'}
      className='max-w-full max-h-[520px] object-contain rounded'
    />
  </div>
)}
```

**步骤 4：运行前端检查**

运行：

```bash
cd AionUi
bun run typecheck
```

预期：PASS。如果没有该脚本：

```bash
cd AionUi
bun run build
```

预期：PASS。

**步骤 5：提交**

运行：

```bash
cd AionUi
git add packages/desktop/src/common/types/platform/acpTypes.ts packages/desktop/src/renderer/pages/conversation/Messages/acp/MessageAcpToolCall.tsx
git commit -m "feat: preview codex acp generated images"
```

---

### 任务 6：前端兜底清洗超大 ACP raw output

**文件：**

- 修改：`AionUi/packages/desktop/src/common/chat/chatLib.ts`

**步骤 1：增加防御性 sanitizer**

在 `mergeAcpToolCallContent` 前新增：

```ts
const INLINE_IMAGE_RESULT_LIMIT = 64 * 1024;

function sanitizeAcpToolUpdate<T extends { rawOutput?: any; raw_output?: any }>(update: T): T {
  const next = { ...update };
  for (const key of ['rawOutput', 'raw_output'] as const) {
    const raw = next[key];
    if (!raw || typeof raw !== 'object') continue;

    const result = raw.result;
    const savedPath = raw.saved_path;
    if (typeof result !== 'string' || result.length <= INLINE_IMAGE_RESULT_LIMIT || typeof savedPath !== 'string') {
      continue;
    }

    next[key] = {
      ...raw,
      result: undefined,
      image: raw.image || {
        path: savedPath,
        mime_type: 'image/png',
        source: 'codex_image_generation',
      },
      result_omitted: true,
      result_omitted_reason: raw.result_omitted_reason || 'image_base64',
      result_bytes: raw.result_bytes || result.length,
    };
    delete next[key].result;
  }
  return next;
}
```

然后修改 merge：

```ts
update: sanitizeAcpToolUpdate({
  ...existing.update,
  ...incoming.update,
}),
```

**步骤 2：运行前端检查**

运行：

```bash
cd AionUi
bun run typecheck
```

预期：PASS。

**步骤 3：提交**

运行：

```bash
cd AionUi
git add packages/desktop/src/common/chat/chatLib.ts
git commit -m "fix: guard acp tool output size in renderer"
```

---

### 任务 7：文档同步

**文件：**

- 修改：`AionCLI/ARCHITECTURE.zh-CN.md`
- 修改：`AionUi/docs/guides/webui.md` 或新增 `AionUi/docs/guides/acp-image-output.md`

**步骤 1：更新 AionCLI 架构文档**

在 ACP 事件或 agent runtime 相关章节补充：

```markdown
### ACP 工具输出清洗

Codex ACP 的图片生成工具可能返回 `saved_path` 与 inline image base64。AionCLI 在 ACP 翻译边界会把图片 base64 从 `raw_output.result` 中移除，只保留 `saved_path`、`image.path`、`result_omitted` 与大小元数据，避免 WebSocket、SQLite 和前端渲染承载大 payload。
```

**步骤 2：更新 AionUi 展示文档**

如果 `webui.md` 适合补充，则新增一小节；否则新增指南：

```markdown
# ACP 图片输出展示

AionUi 对 ACP 工具调用中的 `raw_output.image.path` / `raw_output.saved_path` 渲染本地图片预览。前端不依赖 inline base64，图片文件通过 `/api/fs/image-base64` 按需读取。
```

**步骤 3：提交文档**

运行：

```bash
cd AionCLI
git add ARCHITECTURE.zh-CN.md docs/plans/2026-05-19-codex-image-acp-output.md
git commit -m "docs: document acp image output handling"
```

运行：

```bash
cd AionUi
git add docs/guides/webui.md
git commit -m "docs: document acp image preview handling"
```

如果实际新增了 `docs/guides/acp-image-output.md`，提交该文件。

---

### 任务 8：本地回归验证

**文件：**

- No code changes.

**步骤 1：后端测试**

运行：

```bash
cd AionCLI
cargo test -p aionui-ai-agent codex_image_tool_update
cargo test -p aionui-conversation acp_image_tool_call
cargo test -p aionui-ai-agent
cargo test -p aionui-conversation
```

预期：PASS。

**步骤 2：前端测试**

运行：

```bash
cd AionUi
bun run typecheck
bun run build
```

预期：PASS。

**步骤 3：静态检查**

运行：

```bash
cd AionCLI
cargo fmt --check
cargo clippy -p aionui-ai-agent -p aionui-conversation --all-targets -- -D warnings
git diff --check
```

预期：PASS。

运行：

```bash
cd AionUi
git diff --check
```

预期：PASS。

**步骤 4：手工复现验证**

启动本地 AionUi/AionCLI 后，用 Codex 会话发送：

```text
帮我生成一只小猫
```

预期：

- 不再卡在持续执行中。
- DB 中 `acp_tool_call` 消息小于 `100KB`。
- `raw_output.result` 不存在。
- `raw_output.image.path` 或 `raw_output.saved_path` 存在。
- 前端工具卡直接显示生成图片。

检查 DB：

```bash
sqlite3 "$HOME/Library/Application Support/AionUi/aionui/aionui-backend.db" \
  "select id,status,length(content),instr(content,'iVBORw0KGgo'),instr(content,'result_omitted') from messages where type='acp_tool_call' order by created_at desc limit 5;"
```

预期：

- `status` 为 `finish`
- `length(content)` 不再是 MB 级
- `instr(content,'iVBORw0KGgo')` 为 `0`
- `instr(content,'result_omitted')` 大于 `0`

**步骤 5：最终状态检查**

运行：

```bash
git -C AionCLI status --short --branch
git -C AionUi status --short --branch
```

预期：

- 只有预期提交或干净工作区。
- 不包含 co-author 元信息。

---

## 风险与回滚

- 风险：某些非图片工具也可能返回超长字符串并带 `saved_path`。缓解：清洗条件同时要求图片 base64 前缀或 `data:image/`。
- 风险：Codex 后续字段名变化。缓解：前端同时兼容 `rawOutput` 和 `raw_output`，后端只依赖 `saved_path/result/status` 这些当前已观测字段。
- 风险：状态归一化误把仍在生成的图片标记完成。缓解：只在存在 `saved_path` 且图片 result 被清洗后归一化为 completed。
- 回滚：后端 sanitizer 是边界纯函数，回滚 `translate.rs` 相关提交即可恢复原始 ACP 事件透传；前端展示改动独立，可单独回滚。
