> *本文是 [20-coding-artifacts.md](20-coding-artifacts.md) 的中文翻译。*

# 20. Claims 作用域 Coding 制品

Coding agent 的可重放产物经 claims-scoped artifact API 存储，支持 `patch`、
`run_transcript` 与 `reproduce_script`；另有 `input_snapshot`，用于在创建 invocation
之前上传不可变的 JSON 输入。内置 `input_ref` 解析器只读取该 kind，且默认关闭（见
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
graph 内的条目；`?kind=<kind>` 只返回该 kind，未知 kind 返回 `400`。
`GET /api/v1/artifacts/{id}/download` 也先执行同一验证。

| `kind` | `Content-Type` | 扩展名 | `task_iri` |
| --- | --- | --- | --- |
| `patch` | `text/x-diff; charset=utf-8` | `.patch` | 必填 |
| `run_transcript` | `text/plain; charset=utf-8` | `.log` | 必填 |
| `reproduce_script` | `text/x-shellscript; charset=utf-8` | `.sh` | 必填 |
| `input_snapshot` | `application/json` | `.json` | 可选 |

给出 `task_iri` 时，它必须非空、不超过 2048 字节、不含 ASCII 控制字符。服务端只保存并回显它：
不检查它是否指向已存在的任务，也不用于授权或查询（制品在调用者 claims 内按 id 读取）。
因此调用方可以填自己的业务 IRI 或 URN。省略时元数据中为 `task_iri: null`。

## 输入快照

`input_snapshot` 是调用方的不可变输入，例如之后某次 invocation 的大输入。内容必须是合法的
UTF-8 JSON（不带 BOM），否则上传返回 `400` 且不落盘；大小上限、明文密钥拦截与 claims 作用域
与其他 kind 相同。

```json
{
  "kind": "input_snapshot",
  "content_base64": "eyJzY2hlbWEiOiJzbmFwc2hvdC92MSJ9"
}
```

启用内置 `wao-artifact://` `input_ref` 取数器（#341）后，invocation 可用 `input_ref` 引用返回的 id 与摘要：`wao-artifact://<project>/<id>`，
`sha256` 取返回的 `artifact.sha256`（即上传字节的 SHA-256）。注意 `input_ref` 有自己的、
更小的内容上限。

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
| `private_key` | `-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----` 或 `… PRIVATE KEY BLOCK-----`（证书、公钥不拦） |
| `aws_access_key_id` | `AKIA` + 16 个 `[0-9A-Z]` |
| `aws_secret_access_key` | `aws_secret_access_key`（不分大小写），可选引号，转义形式（`\"`、`\'`）或未转义形式（`"`、`'`）均可，可选空白，分隔符 `=>`、`:` 或 `=`，再可选空白，以及可选的转义或未转义引号，然后是 40 位 `[A-Za-z0-9/+=]` 值 |
| `github_classic_pat` | `ghp_` + 36 个 `[A-Za-z0-9]` |
| `github_fine_grained_pat` | `github_pat_` + 82 个 `[A-Za-z0-9_]` |
| `slack_token` | `xoxb-`/`xoxa-`/`xoxp-`/`xoxr-`/`xoxs-` + 至少 10 个 `[A-Za-z0-9-]` |
| `sk_api_key` | 左边界为文本开头、`[A-Za-z0-9_-]` 以外的字符、转义的 `\n`、`\r`、`\t`、`\b`、`\f`、`\u` 加四位十六进制，或 `%` 加两位十六进制。随后是 `sk-`、可选的 `proj-` 或 `ant-`，以及至少 20 个 `[A-Za-z0-9_-]`。仅当 `sk-` 之后按 `-` 分成至少三段、其中至少两段是一个或多个 `[a-z]`、且每一段都是一个或多个 `[a-z]`（最后一段也可以是 `v` 加 1 到 4 位数字）时，该命中才是名称而不是密钥。`sk-learn-classification-examples-v2` 和 `sk-my-long-running-service-name` 是名称。其他位置出现数字、更长的数字串、大写字母或 `_` 时仍是密钥。全字母的 key 会放行，这是已接受的风险 |

全字母的 key 会放行：当 `sk-` 之后每一段都是 `[a-z]+` 且满足上面的段数要求时，命中按名称处理。这是已接受的风险。

`task-`、`risk-`、`disk-`、`ask-`、`Slovakia` 等普通文本不会命中；按名字引用凭据
（`AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}`、`TOKEN="$TOKEN_FROM_ENV"`）是允许的。

