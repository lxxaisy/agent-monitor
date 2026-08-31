# Agent Monitor

> A local, read-only macOS menu-bar companion for monitoring Codex tasks in real time.

Agent Monitor 是一个本地、只读的 macOS 菜单栏任务监控器。启动后默认显示一个
右上角常驻的“任务雷达”浮窗，完整控制台保持隐藏；浮窗跨桌面可见，但不会强制
覆盖其他应用。关闭浮窗只会隐藏它，不会停止后台监控。首版连接本机 `codex app-server` 的 stdio
JSON-RPC 通道，按 Codex 返回的 `cwd` 精确分组，展示线程最近一个 turn 的运行、
待处理、空闲、完成、失败和中断状态。

## 当前范围

- 只接入本机 Codex；provider adapter 边界已预留给 Claude Code、Zed、Gemini CLI 等。
- 只读同步，不取消、重试、追问、批准、归档或删除 Codex 任务。
- 初次同步静默，不为历史成功、失败或中断任务重复发通知；终态通知要求此前有运行/待处理状态或实时 session 证据。
- 有 plan 时按 `(completed + 0.5 × inProgress) / total` 显示估算进度；没有 plan 时显示“不可估算”。
- 监控器只在自己的 SQLite 中保存轻量快照和通知去重键，不保存提示词、回答、密钥或原始 JSONL。
- 托盘左键打开任务雷达；托盘菜单可以分别打开任务雷达或完整控制台。任务雷达默认只显示运行中、待处理和今日完成，点击“查看控制台”进入完整控制台。
- 菜单栏图标旁只显示当前运行和待办数量，避免把历史失败/中断误认为正在执行；完整控制台默认使用“实时”筛选，也可主动查看全部记录、失败、中断和归档任务。
- 任务雷达可拖到任意位置并通过右下角调整大小；隐藏后再次打开会保留本次运行中的位置和尺寸。
- macOS 原生通知支持点击后打开任务雷达并定位到对应任务详情；通知权限被拒绝时仍保留浮窗、托盘徽标和应用内提醒。
- 运行中的任务显示缓慢的进度流光，待处理和最近完成使用低频状态呼吸与语义色提示；开启系统“减少动态效果”后自动关闭持续动画。
- 在 macOS 上点击“在 Codex 中打开”会通过 `codex://threads/<taskId>` deeplink 定位到 Codex App；非 macOS 环境使用受控的 `codex resume <taskId>` 参数调用。

## 开发

当前 Tauri 2 依赖要求 Rust `1.77.2` 或更高版本。

运行桌面开发版本前，请准备 macOS 13+（Apple Silicon）、Node.js 18+、pnpm、Rust
以及已登录的 Codex App 或 Codex CLI。监控器只读取本机 Codex 数据，不需要额外的
账号或后端服务。

```bash
pnpm install
pnpm dev
pnpm typecheck
pnpm build
cd src-tauri && cargo test
```

桌面开发需要安装 Tauri 2 的系统依赖，之后运行：

```bash
pnpm tauri:dev
```

生成本地未签名 macOS 安装包：

```bash
pnpm tauri build
```

产物位于 `src-tauri/target/release/bundle/`，其中包含 Apple Silicon 的
`Agent Monitor_0.1.0_aarch64.dmg`。签名、公证和 App Sandbox 尚未启用。

默认数据库位置为：
`~/Library/Application Support/Agent Monitor/monitor.db`

## 开源协议

本项目采用 MIT License，详见 [LICENSE](LICENSE)。

## 同步边界

应用启动一个长期存在的 `codex app-server --listen stdio://` 子进程，先完成
`initialize`/`initialized`，再分页调用 `thread/list`，并按变化读取
`thread/read(includeTurns=false)`，随后用
`thread/turns/list(itemsView=notLoaded, sortDirection=desc, limit=1)` 读取最新
turn 的轻量元数据。监控器不会常规读取完整 turns、items、提示词、回答或命令
输出。归档线程始终只使用 `thread/list` 元数据，不逐个读取历史 turn；从 SQLite
恢复的当前任务只在用户打开详情时按需补充最新 turn 元数据。

Codex 的 `source` 字符串或对象会在 Rust Core 中统一为稳定的来源类型。计划进度
优先来自 API；`turn/plan/updated` 事件和 session `update_plan` 只建立按 turn ID
隔离的轻量缓存，置信度依次为高、中、低。缓存只保留步骤标题和状态在内存中，
SQLite 仅保存步骤计数与估算结果。后台同时监听
`~/.codex/sessions/**/*.jsonl` 的追加和轮换，并每 2 秒轻量核对已知文件的长度与
文件身份，补足 macOS 文件事件漏报；只对发生变化的文件继续增量读取。每 15 秒执行
一次 API 校准。连接失败会
进入离线/降级状态并指数退避，不把连接问题误报成任务失败。

其他 Codex 进程的 app-server 视图可能短暂落后，因此 session 中的结构化
`task_started`、turn 活动、`request_user_input` 和终止事件会形成一层实时状态证据：
活动 turn 可覆盖旧的完成/失败/中断快照，`task_complete` 只撤销实时覆盖，最终终态
仍由 Codex API 判定。启动时仅检查有限文件尾部和仍被 Codex 进程打开的 session，
避免把历史异常退出误报为正在运行；内部 chat passthrough UUID 不作为 Codex turn ID。
该过程不保留提示词、回答或命令输出。

## 重要语义

“已完成”只表示某个 Codex turn 收到 `completed`。“失败”和“中断”严格对应
Codex 最近 turn 的 `failed` 与 `interrupted`，两者不会合并计数。线程 `idle` 只表示当前没有
运行中的 turn，不代表业务项目完成；项目完成度需要未来接入目标、检查清单、Git、
测试或 CI 数据。

## 后续工作

打包签名、公证、macOS App Sandbox、更多 Codex item 类型和通用
`agent-monitor emit` 事件协议属于后续阶段。

欢迎提交 issue 和 pull request。涉及状态映射、通知去重或 session 解析的改动，
请同时补充对应的 Rust 测试。
