> *本文是 [20-coding-artifacts.md](20-coding-artifacts.md) 的中文翻译。*

# 20. Claims 作用域 Coding 制品

Coding agent 的可重放产物经 claims-scoped artifact API 存储，支持 `patch`、
`run_transcript`、`reproduce_script` 与 `input_snapshot`。内置的调用
`input_ref` 解析器只读取 `input_snapshot`，且该解析器默认关闭（见
[29-invocations-api.zh.md](29-invocations-api.zh.md)）。每次上传必须具有 JWT 验证的
`IsolationClaims`；开发用 `X-Identity`、API key 和匿名请求均返回 `401`。

## API

`POST /api/v1/artifacts` 接收：

```json
{
  "kind": "patch",
  "task_iri": "iri://task/123",
  "content_base64": "ZGlmZiAtLWdpdA=="
}
```

响应给出不可变元数据和下载 URL。`GET /api/v1/artifacts` 仅列出调用者 claims
graph 内的条目；`GET /api/v1/artifacts/{id}/download` 也先执行同一验证。

制品字节通过现有 BlobStore 使用服务端 mint 的 `{tenant}/artifacts/` 前缀。
元数据作为 RDF literal 存于调用者的 `graph://{tenant}/{project}`，包括
`task_iri`、内容 hash、创建者、时间和 blob key。客户端数据不能选择任一存储目标。

## 重放与安全

`task_iri` 把 patch、轨迹或脚本与 checkpoint/task 执行关联。复现脚本必须从环境变量
或 secret manager 获得凭据；元数据不会返回 secret。此 API 不读取或迁移历史 blob 对象。

### 明文密钥拦截

内容中嵌入了凭据**值**时，上传返回 `400`，响应体固定为 `{"error": "plaintext secrets are
forbidden in coding artifacts", "code": "artifact_plaintext_secret"}`。不回显命中的文本，也不回显
命中的规则，且不落盘。规则按真实凭据格式匹配，不是裸子串匹配，大小写与真实格式一致。技能流水线的
文件扫描用的是同一个检测模块（`utils::secret_scan`）：命中私钥则门禁失败，命中其他格式只告警。

| 规则 | 匹配 |
| --- | --- |
| `private_key` | `-----BEGIN [A-Z ]*PRIVATE KEY-----` 或 `… PRIVATE KEY BLOCK-----`（证书、公钥不拦） |
| `aws_access_key_id` | `AKIA` + 16 个 `[0-9A-Z]` |
| `aws_secret_access_key` | `aws_secret_access_key`（不分大小写），可选引号，`:`/`=`，再跟 40 位 `[A-Za-z0-9/+=]` 值 |
| `github_classic_pat` | `ghp_` + 36 个 `[A-Za-z0-9]` |
| `github_fine_grained_pat` | `github_pat_` + 82 个 `[A-Za-z0-9_]` |
| `slack_token` | `xoxb-`/`xoxa-`/`xoxp-`/`xoxr-`/`xoxs-` + 至少 10 个 `[A-Za-z0-9-]` |
| `sk_api_key` | 前面不是 `[A-Za-z0-9_-]` 的 `sk-`，后跟至少 20 个 `[A-Za-z0-9_-]`（覆盖 `sk-proj-…`、`sk-ant-…`、`sk-<slug>-<hex>`） |

`task-`、`risk-`、`disk-`、`ask-`、`Slovakia` 等普通文本不会命中；按名字引用凭据
（`AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}`、`TOKEN="$TOKEN_FROM_ENV"`）是允许的。

