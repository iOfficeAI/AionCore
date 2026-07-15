# 架构文档

AionCore 是 AionUi 的后端服务，使用 Rust 构建（Axum + Tokio + SQLite）。
它通过 HTTP REST API 和 WebSocket 实时事件为 AionUi 桌面客户端提供服务。

## 技术栈

| 组件 | 技术 |
|------|------|
| Web 框架 | Axum 0.8 |
| 异步运行时 | Tokio |
| 数据库 | SQLite（通过 sqlx，异步） |
| 认证 | JWT + CSRF（双提交 Cookie 模式） |
| 实时通信 | WebSocket + 事件广播 |

## 整体架构

```
┌─────────────────────────────────────────────────┐
│                  aionui-app                      │
│            （二进制入口，路由组装）                  │
├──────────┬──────────┬──────────┬────────────────┤
│conversa- │ channel  │  team    │  ...（领域层）   │
│  tion    │          │          │                 │
├──────────┴──────────┴──────────┴────────────────┤
│   aionui-auth          aionui-realtime           │
│  （JWT、CSRF、中间件）  （WebSocket、事件广播）      │
├─────────────────────────────────────────────────┤
│  aionui-db    aionui-api-types   aionui-runtime  │
│  （仓库层）    （API 契约）       （运行时/子进程）   │
├─────────────────────────────────────────────────┤
│       aionui-common          aionui-assets       │
│  （错误类型、枚举、加密）      （嵌入式数据）        │
└─────────────────────────────────────────────────┘
```

依赖方向严格向下流动。领域 crate 不可依赖 aionui-app，
aionui-common 没有任何内部依赖。

## Crate 层级

项目采用 Cargo workspace 组织，共 20 个 crate，分为四层：

### 基础层（Foundation）

被几乎所有其他 crate 依赖，变更需谨慎。

| Crate | 职责 |
|-------|------|
| `aionui-common` | 共享错误类型（ApiError）、枚举、ID 生成、加密工具、时间戳、分页 |
| `aionui-api-types` | 所有 HTTP/WebSocket 的请求和响应类型，是 API 契约的唯一定义处 |
| `aionui-db` | SQLite 数据库层，定义 Repository trait 和实现 |
| `aionui-assets` | 嵌入式静态资源（Agent 元数据、提示词） |
| `aionui-runtime` | 托管 Node、子进程管理、PATH 增强 |

### 能力层（Capability）

提供跨领域的通用能力。

| Crate | 职责 |
|-------|------|
| `aionui-auth` | JWT 认证、密码哈希、CSRF 保护、Cookie 管理、认证中间件 |
| `aionui-realtime` | WebSocket 连接管理、事件广播（BroadcastEventBus）、消息路由 |

### 领域层（Domain）

每个 crate 拥有独立的业务领域，彼此之间保持松耦合。

| Crate | 职责 |
|-------|------|
| `aionui-conversation` | 对话管理、消息收发、确认机制、流式响应 |
| `aionui-channel` | 多渠道集成（微信、钉钉、飞书）、插件系统、配对会话 |
| `aionui-team` | 团队协作、任务调度、邮箱系统 |
| `aionui-cron` | 定时任务执行、Cron 表达式、事件触发 |
| `aionui-file` | 文件操作、监听、快照、Git 操作、压缩 |
| `aionui-office` | Office 文档处理（Excel、PPT、Word）、预览、转换 |
| `aionui-system` | 系统设置、提供商管理、版本检查、模型获取 |
| `aionui-mcp` | MCP 协议集成、OAuth、多平台适配器 |
| `aionui-ai-agent` | Agent 生命周期管理、Worker 任务队列、ACP/辅助技能 |
| `aionui-extension` | 扩展注册中心、Hub 管理、技能发现与安装 |
| `aionui-shell` | Shell 命令执行、语音转文字 |
| `aionui-assistant` | Assistant 配置与管理 |

### 组装层（Composition）

| Crate | 职责 |
|-------|------|
| `aionui-app` | 顶层二进制入口，组装所有 crate 为完整的 Axum 服务 |

### 依赖方向规则

```
组装层 → 领域层 → 能力层 → 基础层
         领域层 → 基础层（可跨层依赖）
```

- ✅ 上层可以依赖下层
- ✅ 同层之间可通过 trait 抽象交互（如 conversation 通过 IWorkerTaskManager trait 使用 ai-agent 的能力）
- ❌ 禁止下层依赖上层
- ❌ 禁止循环依赖

## 领域 Crate 内部结构

每个领域 crate 遵循统一的内部组织模式，以 aionui-conversation 为参考示例：

### 标准目录结构

```
crates/aionui-conversation/src/
├── lib.rs       # 模块导出，定义 crate 的公共 API
├── routes.rs    # HTTP 路由处理函数
├── service.rs   # 业务逻辑层
├── state.rs     # RouterState 结构体（持有 service 和依赖）
├── error.rs     # 领域特定的错误类型（可选）
├── types.rs     # 领域模型（可选）
└── [其他模块]    # 功能特定的子模块（如 streaming.rs）
```

### 各文件职责

**lib.rs** — Crate 入口，只做模块声明和公共 API 导出：
- 导出 `domain_routes()` 函数
- 导出 `Service` 和 `RouterState`
- 不包含业务逻辑

**routes.rs** — HTTP 路由定义和 handler 函数：
- 导出一个 `domain_routes(state: RouterState) -> Router` 函数
- 每个 handler 负责：提取参数 → 调用 service → 构造响应
- handler 不包含业务逻辑，只做请求/响应转换

**service.rs** — 业务逻辑的唯一存放处：
- 通过构造函数注入依赖（Repository trait 对象、EventBroadcaster 等）
- 所有业务规则、校验、编排逻辑都在这里
- 不直接接触 HTTP 类型（不导入 axum）

**state.rs** — 路由状态，是依赖注入的载体：
- 持有 service 实例和其他依赖的 Arc 引用
- 实现 Clone（Axum 要求）

### Handler 签名约定

```rust
async fn handler(
    State(state): State<RouterState>,       // 依赖注入
    Extension(user): Extension<CurrentUser>, // 当前认证用户
    Path(id): Path<String>,                  // 路径参数
    Json(body): Json<RequestType>,           // 请求体
) -> Result<(StatusCode, Json<ApiResponse<ResponseType>>), ApiError>
```

### 何时新建 Crate vs 扩展现有 Crate

**新建 crate：**
- 代表一个独立的业务领域（有自己的数据模型和生命周期）
- 需要独立的路由前缀（如 `/api/new-domain/...`）
- 与现有领域没有强耦合关系

**扩展现有 crate：**
- 功能属于已有领域的子功能
- 共享同一组数据模型
- 路由是现有前缀的子路径

## API 规范

### RESTful 路径命名

```
/api/{资源复数}                    # 集合操作（GET 列表、POST 创建）
/api/{资源复数}/{id}               # 单体操作（GET 详情、PATCH 更新、DELETE 删除）
/api/{资源复数}/{id}/{子资源复数}    # 嵌套资源
/api/{资源复数}/{id}/{动作}         # 动作操作（仅在 CRUD 无法表达时使用）
```

规则：
- 始终使用 `/api/` 前缀
- 资源名和路径段使用 kebab-case（如 `ai-agents`、`qr-login`）
- 动作类路由使用动词或动词短语（如 `reset`、`stop`、`run`）

### 统一响应格式

**成功响应（`ApiResponse<T>`）：**
```json
{
  "success": true,
  "data": { ... },
  "message": "optional message"
}
```
`data` 和 `message` 均为可选字段，值为 null 时不序列化。

**错误响应（`ErrorResponse`）：**
```json
{
  "success": false,
  "error": "Human-readable error message",
  "code": "ERROR_CODE"
}
```

所有响应类型定义在 `aionui-api-types` 中，这是 API 契约的唯一来源。

### HTTP 状态码映射

| ApiError 变体 | 状态码 | 错误码 | 使用场景 |
|---------------|--------|--------|----------|
| BadRequest | 400 | BAD_REQUEST | 请求参数无效 |
| Unauthorized | 401 | UNAUTHORIZED | 未认证或 token 过期 |
| Forbidden | 403 | FORBIDDEN | 无权限访问 |
| NotFound | 404 | NOT_FOUND | 资源不存在 |
| Conflict | 409 | CONFLICT | 资源冲突 |
| UnprocessableEntity | 422 | UNPROCESSABLE_ENTITY | 语义错误 |
| RateLimited | 429 | RATE_LIMITED | 请求频率超限 |
| Internal | 500 | INTERNAL_ERROR | 服务器内部错误 |
| BadGateway | 502 | BAD_GATEWAY | 上游服务异常 |
| Timeout | 502 | TIMEOUT | 上游服务超时 |

### 分页

使用偏移分页方式（`PaginatedResult<T>`）：

```json
{
  "items": [...],
  "total": 100,
  "hasMore": true
}
```

字段说明：
- `items` — 当前页数据
- `total` — 总记录数
- `hasMore` — 是否还有更多数据

注：JSON 字段名使用 camelCase（通过 `#[serde(rename_all = "camelCase")]`）。

### WebSocket 事件规范

**入口：** 单一 `/ws` 端点

**消息格式（`WebSocketMessage<T>`）：**
```json
{
  "name": "domain.actionName",
  "data": { ... }
}
```

**事件命名规范：**
- 格式：`{domain}.{actionName}`，两级结构
- domain 使用 camelCase（如 `conversation`、`fileWatch`）
- actionName 使用 camelCase（如 `listChanged`、`statusChanged`）
- 示例：`conversation.listChanged`、`cron.jobExecuted`、`extensions.stateChanged`

⚠️ **遗留说明：** 部分现有事件使用 kebab-case（如 `channel.pairing-requested`）
或三级命名（如 `team.agent.status`）。这些是历史遗留，
新增事件必须遵循上述两级 camelCase 规范，
现有不一致的事件在相关模块迭代时逐步统一。

### ACP 工具输出清洗

ACP Agent 的工具调用事件在 `aionui-ai-agent` 翻译层进入统一
`AgentStreamEvent`。该边界必须保证 WebSocket 和 SQLite 消息内容
是可控大小，不能把工具返回的大型二进制或 inline base64 原样透传。

Codex 图片生成工具可能同时返回 `saved_path` 和 `raw_output.result`
中的 PNG/JPEG/WebP base64。翻译层会在转发和持久化之前移除
`result`，保留 `saved_path`、`image.path`、`result_omitted`、
`result_bytes` 等小型结构化字段；如果图片已经落盘，则把工具状态
归一化为 `completed`。前端应通过文件路径按需加载图片，不应依赖
inline base64。

## 数据层

### Repository Trait 模式

所有数据库访问通过 trait 抽象，定义在 `aionui-db` 中：

```rust
#[async_trait]
pub trait IConversationRepository: Send + Sync {
    async fn get(&self, id: &str) -> Result<Option<ConversationRow>, DbError>;
    async fn create(&self, row: &ConversationRow) -> Result<(), DbError>;
    async fn update(&self, id: &str, params: &UpdateConversationParams) -> Result<(), DbError>;
    async fn delete(&self, id: &str) -> Result<(), DbError>;
    // ...
}
```

规则：
- 每个领域实体对应一个 Repository trait（如 `IConversationRepository`、`IUserRepository`）
- trait 命名以 `I` 开头，表示接口
- 具体实现使用 `Sqlite` 前缀（如 `SqliteConversationRepository`）
- Service 层只依赖 trait，不依赖具体实现

### 类型分布

项目中有三类数据类型，各有归属：

| 类型 | 位置 | 用途 | 示例 |
|------|------|------|------|
| Row 模型 | `aionui-db/src/models/` | 数据库行映射 | `ConversationRow` |
| Params 对象 | `aionui-db/src/repository/` | 数据库写入参数 | `UpdateConversationParams` |
| 请求/响应类型 | `aionui-api-types` | API 契约与共享 DTO | `CreateConversationRequest`、`ConversationResponse` |

**Service 层可以直接使用 `aionui-api-types` 中的类型。** 该 crate 是纯数据结构定义，
不依赖任何 HTTP 框架，本质上是共享 DTO 层。

⚠️ **关键约束：`aionui-api-types` 禁止依赖 axum、tower 等 HTTP 框架，
只允许 serde 和基础类型依赖。** 这是 Service 层能安全使用它的前提。

### 职责分界

- **Handler（routes.rs）**：请求校验、参数提取、错误映射、构造 `ApiResponse`
- **Service（service.rs）**：业务逻辑、规则校验、编排 Repository 调用、Row ↔ Response 转换
- **Repository（aionui-db）**：纯数据库操作，不包含业务逻辑

Handler 与 Service 的分界线是**职责**而非类型——
Handler 不做业务判断，Service 不做 HTTP 处理。

### Migration 管理

使用 sqlx 的内嵌迁移（`sqlx::migrate!()`）：
- 迁移文件位于 `crates/aionui-db/migrations/`
- 命名格式：`NNN_descriptive_name.sql`（序号递增）
- 迁移在应用启动时自动执行
- 新增表或字段变更必须通过迁移文件，禁止手动修改数据库
- 使用 `IF NOT EXISTS` 保证幂等性

### 错误传播

```
DbError（数据库层）
  ↓ From trait 实现（aionui-db/src/error.rs）
ApiError（统一错误类型）
  ↓ IntoResponse 实现
HTTP 响应（状态码 + ErrorResponse JSON）
```

映射规则：
- `DbError::NotFound` → `ApiError::NotFound`（保留语义）
- `DbError::Conflict` → `ApiError::Conflict`（保留语义）
- `DbError::Query` / `Migration` / `Init` → `ApiError::Internal`（屏蔽内部细节）

## 依赖注入

### 注入链路

应用使用 Axum 的 `with_state()` 模式实现依赖注入，分三步完成：

**第一步：集中构建服务（AppServices）**

`aionui-app` 中定义 `AppServices`，集中持有所有共享依赖：

```rust
pub struct AppServices {
    pub database: Database,
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub cookie_config: Arc<CookieConfig>,
    pub qr_token_store: Arc<QrTokenStore>,
    pub ws_manager: Arc<WebSocketManager>,
    pub event_bus: Arc<BroadcastEventBus>,
    pub worker_task_manager: Arc<dyn IWorkerTaskManager>,
    pub agent_registry: Arc<AgentRegistry>,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub acp_session_sync: Arc<AcpSessionSyncService>,
    pub jwt_secret_raw: String,
    pub data_dir: String,
    pub local: bool,
    pub app_version: String,
    pub skill_paths: Arc<SkillPaths>,
    pub guide_mcp_config: Option<GuideMcpConfig>,
    // ...
}
```

**第二步：按领域构建 RouterState**

`build_module_states()` 从 `AppServices` 构建出所有领域的 RouterState。
每个领域只获取自己需要的依赖：

```rust
// 简单的领域——只需要一个 service
pub struct CronRouterState {
    pub cron_service: Arc<CronService>,
}

// 复杂的领域——需要多个 service
pub struct OfficeRouterState {
    pub watch_manager: Arc<OfficecliWatchManager>,
    pub snapshot_service: Arc<SnapshotService>,
    pub conversion_service: Arc<ConversionService>,
    pub proxy_service: Arc<ProxyService>,
}
```

所有 RouterState 都是 `#[derive(Clone)]`，持有 Arc 包装的依赖。

**第三步：Handler 通过 State 提取依赖**

```rust
async fn create(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), ApiError> {
    let Json(req) = body.map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let conversation = state.conversation_service.create(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}
```

### 路由组装

路由组装通过三层函数完成：

1. `create_router()` — 异步入口，构建所有 state 后调用下层
2. `create_router_with_states()` — 允许自定义 ModuleStates（便于测试）
3. `create_router_with_all_state()` — 最终组装，合并所有路由和中间件

中间件栈（由外到内）：

```
CORS（仅 local 模式）
  → Security Headers（所有请求）
    → CSRF（仅非 local 模式）
      → Auth Middleware（按路由组选择性应用）
        → Handler
```

关键点：
- 认证中间件不是全局的，而是通过 `route_layer()` 按路由组选择性应用
- 登录、状态检查等公开路由不附加认证中间件
- WebSocket `/ws` 路由不走 HTTP 认证中间件，使用独立的 token 校验回调
- local 模式下跳过 CSRF 检查，注入默认系统用户

### 规则

- **AppServices 是唯一的服务构建中心**——所有 Repository 实例化和 Service 组装在此完成
- **RouterState 只持有必要的依赖**——每个领域的 State 只包含自己用到的 service
- **依赖通过 `Arc<dyn Trait>` 传递**——支持运行时多态和测试替换
- **领域 crate 不负责构建自己的依赖**——只定义需要什么（RouterState），由 `aionui-app` 负责组装

## 安全模型

### 中间件栈（由外到内）

```
CORS（仅 local 模式，允许任意来源）
  → Security Headers
      X-Frame-Options: DENY
      X-Content-Type-Options: nosniff
      X-XSS-Protection: 1; mode=block
      Referrer-Policy: strict-origin-when-cross-origin
    → CSRF（仅非 local 模式，Double Submit Cookie）
      → Auth Middleware（按路由组选择性应用）
        → Handler
```

### JWT 认证

- 算法：HMAC-SHA256
- 有效期：24 小时
- Payload：`user_id`、`username`、`iat`、`exp`、`iss`（"aionui"）、`aud`（"aionui-webui"）
- Secret 来源优先级：环境变量 → 数据库 → 随机生成（64 字节，getrandom）
- Token 提取优先级：`Authorization: Bearer` 头 → `aionui-session` Cookie
- 支持 Token 黑名单（SHA-256 哈希，DashMap 存储）

### CSRF 保护

采用 Double Submit Cookie 模式：
- Cookie 名：`aionui-csrf-token`（非 HttpOnly，JavaScript 需读取）
- 请求头：`x-csrf-token`
- 校验逻辑：Cookie 值必须与请求头值完全匹配
- 安全方法（GET、HEAD、OPTIONS）免校验
- 豁免路径：`/login`、`/api/auth/qr-login`

### 密码安全

- 算法：bcrypt，cost factor 12
- 计时攻击防护：最低 50ms 响应时间
- 用户枚举防护：用户不存在时使用预计算 dummy hash 消耗等量时间

### Cookie 配置

| Cookie | HttpOnly | Secure | SameSite | Max-Age |
|--------|----------|--------|----------|---------|
| `aionui-session` | ✅ | HTTPS 时 | Strict(HTTPS) / Lax(HTTP) | 30 天 |
| `aionui-csrf-token` | ❌ | HTTPS 时 | Strict(HTTPS) / Lax(HTTP) | 30 天 |

### 频率限制

| 级别 | 限制 | 窗口 | 应用范围 | Key |
|------|------|------|----------|-----|
| Auth | 5 次失败 | 15 分钟 | 登录路由 | 客户端 IP |
| API | 60 次请求 | 1 分钟 | 公开端点 | 客户端 IP |
| Action | 20 次请求 | 1 分钟 | 敏感操作 | 用户 ID（降级为 IP） |

IP 提取优先级：`X-Forwarded-For` → `X-Real-IP` → "unknown"

### Local 模式

通过 `--local` 启动标志启用，用于 Electron 嵌入场景：
- 跳过 JWT 验证，注入固定用户（`system_default_user`）
- 跳过 CSRF 检查
- 启用全开放 CORS
- WebSocket 同样免认证

### 安全规则

- 新增端点必须评估是否需要认证中间件
- 状态变更操作（POST/PUT/DELETE/PATCH）必须受 CSRF 保护
- 敏感操作应配置频率限制
- 错误响应禁止泄露内部实现细节（DbError::Query 映射为通用 Internal）
- Secret 禁止硬编码在代码中

## 测试策略

### 测试分层

| 层级 | 位置 | 数据库策略 | 用途 |
|------|------|-----------|------|
| 单元测试 | 各 `.rs` 文件内 `#[cfg(test)]` | 无或 Mock | 函数级逻辑验证 |
| 集成测试 | `crates/<crate>/tests/` | 内存 SQLite | Service 和 Repository 行为验证 |
| E2E 测试 | `crates/aionui-app/tests/` | 内存 SQLite | 完整 HTTP 请求链路验证 |

### 内存数据库

所有需要数据库的测试使用 `init_database_memory()`：
- 创建 SQLite 内存数据库（`sqlite::memory:`）
- 单连接池（`max_connections = 1`，保证内存库数据一致性）
- 自动执行迁移
- 自动创建系统默认用户（`system_default_user`）
- 每个测试获得独立的全新数据库实例

### Mock 策略

**优先使用真实内存数据库，Mock 仅用于隔离不需要的依赖。**

- 集成测试和 E2E 测试：使用真实 Sqlite 实现 + 内存数据库
- 单元测试：对不相关的依赖使用 Mock（如 `MockBroadcaster`、`MockConversationRepo`）
- Mock 实现使用 `Mutex<Vec<T>>` 做内存存储，手动实现 trait

### E2E 测试模式

`aionui-app/tests/common/mod.rs` 提供共享的测试工具：

```rust
// 构建完整应用
let (app, services) = build_app().await;

// 创建用户并登录，获取认证凭据
let (token, csrf) = setup_and_login(&services, "testuser", "password").await;

// 发起认证请求
let response = app.oneshot(
    get_with_token("/api/conversations", &token, &csrf)
).await;
```

登录流程：
1. 直接通过 Repository 创建用户（绕过 API）
2. GET `/api/auth/status` 提取 CSRF token
3. POST `/login` 获取 session token
4. 后续请求携带 `Authorization: Bearer` + `x-csrf-token` 头

### 测试文件命名

| 后缀 | 用途 | 示例 |
|------|------|------|
| `*_test.rs` | 单元/功能测试 | `extension_loading_test.rs` |
| `*_integration.rs` | 集成测试 | `acp_agent_integration.rs` |
| `*_e2e.rs` | 端到端测试 | `auth_e2e.rs`、`conversation_e2e.rs` |

### 测试失败处理规则

测试失败时，禁止直接修改测试来通过。必须先判断：

1. **测试断言仍然代表正确行为** → 修实现代码，不动测试
2. **需求或接口已有意变更，测试反映的是旧行为** → 可以改测试，但必须：
   - 确认变更是有意为之（不是无意的副作用）
   - 新断言仍在验证有意义的行为
3. **不确定** → 停下来，回溯变更，搞清楚再继续

禁止项：
- ❌ 删除失败的测试来"解决"问题
- ❌ 将具体断言改为模糊断言（如 `assert_eq!(status, 201)` → `assert!(status.is_success())`）

## 新增功能指南

### 何时新建 Crate

**新建 crate：**
- 代表独立的业务领域（有自己的数据模型和生命周期）
- 需要独立的路由前缀（`/api/new-domain/...`）
- 与现有领域没有强耦合关系

**扩展现有 crate：**
- 功能属于已有领域的子功能
- 共享同一组数据模型
- 路由是现有前缀的子路径

### 新建领域 Crate 的完整步骤

以添加 `aionui-my-feature` 为例：

**第一步：创建 crate 并注册到 workspace**

1. 创建目录 `crates/aionui-my-feature/`
2. 在根 `Cargo.toml` 中添加 workspace 成员：
   ```toml
   members = [
       # ... 现有成员
       "crates/aionui-my-feature",
   ]
   ```
3. 在根 `Cargo.toml` 的 `[workspace.dependencies]` 中注册：
   ```toml
   aionui-my-feature = { path = "crates/aionui-my-feature" }
   ```
4. crate 内的依赖使用 `.workspace = true` 引用共享版本

**第二步：按标准结构编写 crate**

```
crates/aionui-my-feature/
├── Cargo.toml
├── src/
│   ├── lib.rs        # 导出 my_feature_routes、MyFeatureService、MyFeatureRouterState
│   ├── routes.rs     # pub fn my_feature_routes(state: ...) -> Router
│   ├── service.rs    # 业务逻辑
│   └── state.rs      # #[derive(Clone)] pub struct MyFeatureRouterState { ... }
└── tests/
    └── my_feature_test.rs
```

**第三步：如需数据库，在 aionui-db 中添加**

1. 在 `models/` 添加 Row 模型
2. 在 `repository/` 定义 Repository trait（`I` 前缀）和 Sqlite 实现
3. 在 `migrations/` 添加迁移文件（`NNN_descriptive_name.sql`）

**第四步：如需 API 类型，在 aionui-api-types 中添加**

在 `aionui-api-types` 中定义请求/响应类型，保持 API 契约集中管理。

**第五步：接入 aionui-app**

1. 在 `aionui-app/Cargo.toml` 添加依赖：
   ```toml
   aionui-my-feature.workspace = true
   ```

2. 在 `ModuleStates` 中添加字段：
   ```rust
   pub my_feature: MyFeatureRouterState,
   ```

3. 编写 `build_my_feature_state()` 函数：
   ```rust
   pub fn build_my_feature_state(services: &AppServices) -> MyFeatureRouterState {
       let pool = services.database.pool().clone();
       let repo = Arc::new(SqliteMyFeatureRepository::new(pool));
       MyFeatureRouterState {
           my_feature_service: MyFeatureService::new(repo, services.event_bus.clone()),
       }
   }
   ```

4. 在 `build_module_states()` 中调用：
   ```rust
   my_feature: build_my_feature_state(services),
   ```

5. 在 `create_router_with_all_state()` 中注册路由：
   ```rust
   let my_feature_authenticated = my_feature_routes(states.my_feature)
       .route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));
   
   let router = Router::new()
       // ... 现有路由
       .merge(my_feature_authenticated)
       // ...
   ```

### 检查清单

新增 crate 前确认：
- [ ] crate 内部结构遵循标准模式（lib/routes/service/state）
- [ ] 依赖方向正确（不依赖上层或同层 crate 的具体实现）
- [ ] Repository trait 定义在 aionui-db，实现使用 Sqlite 前缀
- [ ] API 类型定义在 aionui-api-types
- [ ] 路由使用 `/api/` 前缀，资源名 kebab-case
- [ ] 包含对应的测试文件
- [ ] WebSocket 事件遵循 `domain.camelCaseAction` 命名

## 运行时基础设施

### 托管 Node 运行时

内置 ACP adapter 通过 `crates/aionui-runtime/src/node_runtime/` 中的托管
Node 运行时启动。打包版本从 managed-resources bundle 激活 Node；download
模式将固定版本安装到 `{data_dir}/runtime/node`。Adapter 命令携带明确的
Node 可执行文件路径，不依赖环境中的 `PATH`。

### 启动时 PATH 增强

`fn main()` 在 tokio 运行时启动**之前**调用
`aionui_runtime::enhance_process_path()`，使后续所有
`which::which(...)` 和 `Command::new(...)` 继承增强后的 `PATH`。
三层合并优先级：交互式 login-shell `$PATH`（Unix，3 秒超时）→ 当前继承的
PATH → 平台 fallback 目录（`~/.cargo/bin`、`~/.local/bin`、版本管理器安装
目录、Windows `%APPDATA%\npm`、Git、Scoop 等）。
该调用标记为 `unsafe`，因为 Rust 2024 要求 `env::set_var` 在单线程环境执行；
`main()` 将其作为第一条语句以满足此不变量。
启动时会输出 `startup: PATH ready path_segments=… path_len=…` info 日志。

### 子进程 Spawn Builder

新的子进程启动点应通过
`aionui_runtime::Builder::agent(program)`（长期运行的 Agent CLI，
调用者拥有 stdio）或 `aionui_runtime::Builder::clean_cli(program)`
（短期工具，解析输出）创建。两者都设置 `kill_on_drop(true)`
并清除 `NODE_OPTIONS`/`NODE_INSPECT`/`NODE_DEBUG`/`CLAUDECODE`
防止调试环境泄露到子进程。`clean_cli` 额外设置管道 stdio
和 `NO_COLOR=1` + `TERM=dumb` 以避免 ANSI 码干扰输出解析。

禁止使用原始 `tokio::process::Command` 手动实现这些行为——
集中化的 Builder 是更新策略的唯一位置。
