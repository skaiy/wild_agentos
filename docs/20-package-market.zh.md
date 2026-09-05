# 20. Logic / Skill 包市场

包市场是内核提供的版本化目录，用于复用 `FunctionDef` Logic 与 Skill。它不是
Foundry 级 Workshop：不执行上传包中的代码、不创建工作流，也不提供可视化编排。

## 契约

`POST /api/v1/market/packages` 发布不可变的包版本。包必须声明：

- `name` 与 SemVer `version`（`MAJOR.MINOR.PATCH`）
- JSON Schema：`input_schema`、`output_schema`
- `side_effect_level`：`none`、`read`、`write` 或 `execute`
- `visibility`：`private`、`tenant` 或 `system`
- 可选 `functions`（`FunctionDef`）与 `skills`（`SkillMeta`）

`system` 仅供内核包使用，HTTP API 不允许创建。相同发布者、名称和版本再次发布
返回 `409`；发布新内容必须使用新版本。

包内的每个 Skill 都经过与独立 Skill 发布相同的准入门禁。任何一个门禁失败都会以
`422` 拒绝整个包，不会产生部分发布。

## 租户可见性与安装

目录、安装与回滚均要求由 JWT 认证边界签发的 `IsolationClaims`；写操作还要求
`DA` 角色。请求体中的租户字段不会被采用。

- `private`：仅发布租户的同一 project 可查看、安装。
- `tenant`：发布租户内所有 project 可查看、安装。
- `system`：内核可读包，内容只读。

`POST /api/v1/market/packages/:name/install` 为当前租户/project 选择一个可见版本。
`POST /api/v1/market/packages/:name/upgrade` 显式选择较新版本，服务端绝不隐式选择
“latest”；所选版本不大于当前版本时返回 `409`。
`POST /api/v1/market/packages/:name/rollback` 请求体为 `{}`，恢复记录中的前一版本。
安装记录按 tenant/project 隔离。
