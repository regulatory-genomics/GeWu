# 2026-05-02 GeWu Work Graph / Work Panel 实验记录

## 背景

本次实验的起点是一个产品和架构问题：GeWu 里已经有 `todo`、`task`、`subagent`、`SOP`、`tools`、`trace`、`workflow`、`session` 等执行相关概念，但它们分散在不同的数据结构、接口和 UI 中。用户希望这些概念不要继续作为独立面板堆叠，而是优雅地统一成一个 graph/timeline 管理系统，类似 PantheonOS 右侧 timeline/task 管理体验。

核心判断：GeWu 并不缺少执行数据，缺的是统一投影层。

```text
现有事实数据：tasks + child sessions + execution_runs + execution_events + workflow/SOP metadata
统一投影层：WorkGraph = nodes + edges + events + artifacts
前端视图：Timeline | Todo | Tasks | Trace
```

## 实验目标

1. 定义一个共享的 WorkGraph view model，作为 todo/task/subagent/tool/workflow/trace/session 的统一 UI contract。
2. 新增后端聚合接口，把现有 storage/runtime 数据投影成 WorkGraph，而不是马上引入新表。
3. 把右侧 Trace Panel 升级成 Work Panel，默认支持 Timeline，并增加 Todo/Tasks/Trace 分视图。
4. 用真实 session 验证语义是否正确：
   - `ad7a6c49`：验证 Work Panel 可见、SSE 断连问题。
   - `4291968e`：验证 `todowrite` 中的 todo 是否能在 panel 中看到，并和 task/subagent 语义分开。

## 设计原则

### 1. Todo 是意图图，Task 是执行图

本次实验明确区分：

- `todo`：计划、意图、checklist 状态，来源于 `todowrite`。
- `task/subagent`：委托执行状态，来源于 durable task records 和 child sessions。
- `trace/run/tool`：实际执行证据，来源于 execution runs/events。
- `timeline`：人类可读的时间投影。

### 2. 不先改数据库

第一阶段只做 projection：

```http
GET /api/sessions/{id}/work-graph
```

它从现有数据拼出 graph。只有等 UI 证明有用后，再考虑 `work_nodes/work_edges/work_events` 原生表。

### 3. Compact by default, details on demand

默认面板应该展示 compact timeline / todo / task 状态。Raw input/output、trace tree、tool details 应该通过展开或 Trace tab 查看。

## 实施内容

### Phase 1：Core WorkGraph 模型

新增文件：

```text
gewu-core/src/work_graph.rs
```

新增核心类型：

- `WorkGraph`
- `WorkNode`
- `WorkEdge`
- `WorkEvent`
- `WorkArtifact`
- `WorkNodeKind`
- `WorkStatus`
- `WorkEdgeKind`
- `WorkEventKind`

并从：

```text
gewu-core/src/lib.rs
```

导出这些类型。

主要节点类型包括：

```text
session, assistant_turn, todo, task, subagent, tool,
workflow, workflow_node, sop, artifact, decision, trace_run
```

主要边类型包括：

```text
contains, calls, spawns, waits_for, produces, implements,
derived_from, blocks, recovers, trace_of, session_of
```

### Phase 2：WorkGraph 后端聚合 endpoint

新增文件：

```text
gewu-web/src/routes/work_graph.rs
```

新增路由：

```http
GET /api/sessions/{id}/work-graph
```

路由 wiring：

```text
gewu-web/src/routes/mod.rs
gewu-web/src/lib.rs
```

聚合来源：

- `TaskStore::list_child_tasks(session_id)`
- `ExecutionTraceStore::runs_for_session(session_id)`
- `ExecutionTraceStore::events_for_run(run_id)`
- session runtime state

输出结构：

```json
{
  "graph": {
    "root_node_id": "session:<id>",
    "nodes": [],
    "edges": [],
    "events": [],
    "artifacts": []
  }
}
```

状态映射规则：

- task execution status → `WorkStatus`
- run status → `WorkStatus`
- run kind → `WorkNodeKind`

### Phase 3：右侧 Work Panel

修改文件：

```text
gewu-web/static/index.html
```

将右侧 panel 从单纯 `Execution Trace` 升级为：

```text
Work Panel
Timeline | Todo | Tasks | Trace
```

新增前端函数：

- `loadWorkGraph()`
- `renderWorkPanel()`
- `renderWorkTimeline()`
- `renderWorkTodos()`
- `renderWorkTasks()`
- `renderWorkTrace()`
- `createWorkNodeCard()`
- `currentTodoNodes()`

保留旧 trace button 和 `/api/runs/*` 逻辑，以保持 backward compatibility。

## 调试与修复记录

### 问题 1：Work Panel 没显示

现象：用户打开 UI 后没有看到我们讨论的 graph panel。

证据：Playwright console 报错：

```text
ReferenceError: loadWorkGraph is not defined
```

根因：第一次前端改动中，HTML tabs 和调用点进入了文件，但 WorkGraph JS 函数没有完整进入实际服务的 `index.html`。

修复：补齐 `loadWorkGraph/renderWorkPanel/...` 等前端函数。

### 问题 2：右侧 panel 被布局挤出视口

现象：在较窄浏览器宽度下右侧 panel 不明显或不可见。

根因：主 grid layout 没有稳定分配右侧 Work Panel 列。

修复：将布局改为三栏：

```text
sidebar | chat | work panel
```

并在 900px/1280px 下用 Playwright 验证可见。

### 问题 3：刷新页面会导致 session 跑到一半停止

样例 session：

```text
ad7a6c49-8650-4e86-8bcc-b41153bc7906
```

现象：session 在 `write_file` tool call 后停止。

日志证据：

```text
SSE channel closed, stopping stream
```

根因：`gewu-web/src/routes/chat.rs` 中，如果浏览器 SSE receiver 断开，后端会 `break` 出 agent event stream，导致 agent 执行被终止。页面刷新因此会杀掉正在执行的 assistant turn。

修复：SSE channel 断开后，只停止向浏览器发送事件，后端继续消费 agent stream，让 agent/tool execution 跑完。

语义变化：

```text
旧行为：browser disconnect = stop backend execution
新行为：browser disconnect = stop UI subscription only
```

### 问题 4：Todo 不显示在 panel 中

样例 session：

```text
4291968e-acb7-4f5e-ba07-9615f581de89
```

现象：该 session 中创建了 5 个 todo，但 Work Panel 只能看到 tool/task/trace，不能看到 todo 状态。

根因：`todowrite` 只是作为普通 tool run 被投影，tool input 中的 `todos` 数组没有提升为一等 `todo` nodes。

修复：在 WorkGraph projection 中，遇到 `ExecutionRunKind::Tool && name == "todowrite"` 时，将 input JSON 的 `todos` 数组转换为 `WorkNodeKind::Todo` 节点。

每个 todo node 包含：

- title/content
- status 映射：`pending/running/succeeded/failed/canceled/blocked`
- priority
- source = `todowrite`
- edge：root session `contains` todo
- edge：todowrite run `produces` todo

### 问题 5：Todo snapshot 太多，当前状态不清楚

样例 `4291968e` 的后端数据：

```text
nodes: 97
todo snapshots: 39
task/subagent nodes: 12
```

原因：同一个 session 会多次调用 `todowrite` 更新状态，因此后端保留了多个 todo snapshot。Timeline 应该保留历史，但 Todo tab 应该显示“当前状态”。

修复：前端 Todo tab 用 `currentTodoNodes()` 合并 snapshot，默认按 normalized todo title/content 作为 identity。

曾尝试过按 `todo_id` 合并，但发现不同批次都会复用 `1/2/3`，会把 “任务1” 和 “Agent 1” 错误合并。因此改为按 normalized title/content。

## 样例验证

### ad7a6c49

完整 session id：

```text
ad7a6c49-8650-4e86-8bcc-b41153bc7906
```

验证结果：

- WorkGraph endpoint 正常返回 root session + trace run。
- Work Panel 在浏览器中可见。
- 页面刷新不再应该中断后端执行。

### 4291968e

完整 session id：

```text
4291968e-acb7-4f5e-ba07-9615f581de89
```

验证结果：

- WorkGraph endpoint 能投影 todo nodes。
- `todowrite` snapshots 仍保留在 graph/timeline 中。
- Todo tab 只显示合并后的当前 todo 状态。
- Tasks tab 独立显示 task/subagent 执行状态。

## 验证命令

本次实验中多次运行并通过：

```bash
cargo fmt
cargo check -p gewu-core
cargo check -p gewu-web
cargo test -p gewu-web routes::work_graph::tests
cargo test -p gewu-web
node -e "const fs=require('fs'); const html=fs.readFileSync('gewu-web/static/index.html','utf8'); const m=html.match(/<script>([\\s\\S]*)<\\/script>/); if(!m) throw new Error('script not found'); new Function(m[1]); console.log('embedded script syntax ok');"
```

浏览器验证：

- Playwright 打开 `http://127.0.0.1:8088`。
- 验证 `Work Panel` 可见。
- 验证 `Timeline | Todo | Tasks | Trace` tabs 可见。
- 验证 console 无 `loadWorkGraph` 错误。

## 当前局限

1. `sessions/tree` 在服务重启后仍主要依赖进程内 session state，不能完整列出所有历史 session；但 `/work-graph` 可通过完整 session id 从存储中恢复 graph。
2. Todo identity 当前按 normalized title/content 合并，足够解决 `todo_id` 重复问题，但未来最好在 `todowrite` runtime 层给 todo 生成稳定 id。
3. WorkGraph 仍是 projection layer，不是原生持久化 graph。
4. Todo 与 task 的自动绑定还很初级：目前只是同时显示，尚未实现 “todo implements/execution evidence” 的强绑定。

## 后续建议

1. 给 `todowrite` 引入稳定 todo identity，避免仅靠 title 合并。
2. 在 WorkGraph 中增加 `implements` edge：把 task/tool/workflow run 绑定到对应 todo。
3. 让 session tree 能从 storage 恢复历史 session，而不是仅依赖运行时 state。
4. 增加 artifact tab，把文件写入、workflow outputs、reports 等作为 artifact nodes 显示。
5. 加一个 API test，覆盖 `todowrite` → todo nodes projection。

## 提交前整理记录

本次整理时刻的工作被拆成三个可独立理解的提交单元：

1. WorkGraph 后端聚合接口：`gewu-web/src/routes/work_graph.rs`、`gewu-web/src/routes/mod.rs`、`gewu-web/src/lib.rs`。这个提交只负责把现有 task/run/event 数据投影成 `/api/sessions/{id}/work-graph`，并包含聚合状态映射的 focused tests。
2. Work Panel 与刷新不中断修复：`gewu-web/static/index.html`、`gewu-web/src/routes/chat.rs`。这个提交把右侧面板升级成 `Timeline | Todo | Tasks | Trace`，并修复浏览器 SSE 断开导致后端执行停止的问题。
3. 实验记录：`docs/2026-05-02-work-graph-experiment-record.md`。这个提交保留设计动机、问题定位、修复过程、验证命令和仍需改进的方向，方便之后继续做 prefix session resolution、session tree persistence、todo-task binding 和 artifact projection。

这次提交时刻特意没有纳入仓库里其他已存在的改动，例如 runtime、rag、providers、graph、cli 等模块中的修改，因为它们不属于 WorkGraph/Work Panel 实验的最小闭环。
