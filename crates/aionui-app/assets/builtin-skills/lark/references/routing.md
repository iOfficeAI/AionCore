# Lark domain routing

此文件由 `config/domains.json` 生成。先选择覆盖当前执行步骤的最小 domain 集合，再读取对应 `GUIDE.md`；不要预读其他 domain。

## User-facing domains

| Domain | Use for | Do not use for | Required domains |
| --- | --- | --- | --- |
| [`lark-approval`](subskills/lark-approval/GUIDE.md) | 审批待办、审批实例、审批定义；发起、同意、拒绝、转交或撤回审批 | 普通任务和待办 | — |
| [`lark-apps`](subskills/lark-apps/GUIDE.md) | 妙搭、Spark 或 Miaoda 应用开发与托管；部署应用、查询运行日志、管理环境变量和自动化触发器 | 普通软件项目开发；云盘文件上传 | — |
| [`lark-attendance`](subskills/lark-attendance/GUIDE.md) | 查询自己的考勤打卡记录 | 日程或会议安排 | — |
| [`lark-base`](subskills/lark-base/GUIDE.md) | 多维表格、bitable、字段、记录、视图；Base 表单、仪表盘、workflow 和角色权限 | 电子表格单元格；普通云盘文件管理 | — |
| [`lark-calendar`](subskills/lark-calendar/GUIDE.md) | 日程、忙闲、会议室、RSVP；创建或修改未来会议安排 | 已结束的视频会议记录；普通任务 | — |
| [`lark-contact`](subskills/lark-contact/GUIDE.md) | 按姓名或邮箱解析 open_id；按 open_id 查询人员信息 | 组织架构和部门树遍历 | — |
| [`lark-doc`](subskills/lark-doc/GUIDE.md) | 读取和编辑 Docx 或 Wiki 文档正文；操作文档 blocks、图片和附件 | 文档评论和权限；Sheets 或 Base 数据 | — |
| [`lark-drive`](subskills/lark-drive/GUIDE.md) | 云空间文件、文件夹、上传、下载和导入；评论、权限、订阅、版本和密级标签 | 文档正文编辑；Sheets 或 Base 表内数据 | — |
| [`lark-event`](subskills/lark-event/GUIDE.md) | 实时事件订阅和 NDJSON 消费；机器人、长连接或流式事件处理 | 查询静态资源状态 | — |
| [`lark-im`](subskills/lark-im/GUIDE.md) | 消息、群聊、reaction 和聊天附件；交互卡片、加急、群成员和 Feed 置顶 | 邮箱邮件；视频会议会中消息 | — |
| [`lark-mail`](subskills/lark-mail/GUIDE.md) | 邮件读取、搜索、草稿、发送、回复和转发；邮件文件夹、标签、联系人和收信规则 | 即时通讯消息；纯联系人查询 | — |
| [`lark-markdown`](subskills/lark-markdown/GUIDE.md) | 飞书原生 Markdown 文件读取、创建、patch 和 diff | 将 Markdown 导入在线文档；普通云盘管理 | — |
| [`lark-minutes`](subskills/lark-minutes/GUIDE.md) | 搜索妙记和处理 minute_token；上传音视频、读取或编辑妙记转写和摘要 | 按会议定位关联产物；已知 note_id 的会议纪要 | — |
| [`lark-note`](subskills/lark-note/GUIDE.md) | 使用已知 note_id 查询会议纪要详情和原始逐字记录 | 搜索会议或妙记；读取 Docx 正文 | — |
| [`lark-okr`](subskills/lark-okr/GUIDE.md) | OKR 周期、目标、关键结果、对齐和进展 | 普通任务；绩效评估 | — |
| [`lark-openapi-explorer`](subskills/lark-openapi-explorer/GUIDE.md) | 查找和调用 CLI 尚未封装的飞书原生 OpenAPI | 已有 domain guide 或 CLI 命令覆盖的操作 | — |
| [`lark-sheets`](subskills/lark-sheets/GUIDE.md) | 电子表格、工作表、range、单元格、公式和样式；图表、透视表、筛选和财务建模 | 多维表格；按名称搜索云盘中的表格文件 | — |
| [`lark-skill-maker`](subskills/lark-skill-maker/GUIDE.md) | 把飞书 API 操作封装成自定义 lark-cli Skill | 直接执行已有飞书业务操作 | — |
| [`lark-slides`](subskills/lark-slides/GUIDE.md) | 创建、读取和编辑飞书幻灯片及页面 | 独立白板；普通文件上传下载 | — |
| [`lark-task`](subskills/lark-task/GUIDE.md) | 普通任务、清单、子任务、提醒和协作者；任务附件和任务智能体 | 审批待办 | — |
| [`lark-vc`](subskills/lark-vc/GUIDE.md) | 搜索已结束会议；查询会议纪要、逐字稿、待办和参会人快照 | 进行中的会议和会中事件；未来日程 | — |
| [`lark-vc-agent`](subskills/lark-vc-agent/GUIDE.md) | 加入或离开正在进行的会议；读取会中事件、发言状态、共享内容和会中消息 | 已结束会议、纪要和录制查询 | — |
| [`lark-whiteboard`](subskills/lark-whiteboard/GUIDE.md) | 读取、创建或编辑飞书白板；使用白板图表 DSL | 幻灯片页面内的流程图 | — |
| [`lark-wiki`](subskills/lark-wiki/GUIDE.md) | 知识空间、空间成员和 Wiki 节点；组织、移动或复制知识库文档节点 | 编辑节点内的文档正文；上传普通文件 | — |
| [`lark-workflow-meeting-summary`](subskills/lark-workflow-meeting-summary/GUIDE.md) | 汇总一段时间内的会议纪要；生成会议周报或结构化会议报告 | 查询单个已知会议 | lark-vc |
| [`lark-workflow-standup-report`](subskills/lark-workflow-standup-report/GUIDE.md) | 汇总指定日期的日程与未完成任务；生成今日、明日或本周安排摘要 | 只查询日程或只查询任务 | lark-calendar；lark-task |

## Internal support domains

仅在当前步骤确实涉及对应支持能力时加载。

| Domain | Use for |
| --- | --- |
| [`lark-shared`](subskills/lark-shared/GUIDE.md) | CLI 配置、认证、identity、scope 和 app 权限 |
