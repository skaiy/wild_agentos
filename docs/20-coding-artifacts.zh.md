> *本文是 [20-coding-artifacts.md](20-coding-artifacts.md) 的中文翻译。*

# 20. Claims 作用域 Coding 制品

Coding agent 的可重放产物经 claims-scoped artifact API 存储，支持 `patch`、
`run_transcript` 与 `reproduce_script`；另有 `input_snapshot`，用于在创建 invocation
之前上传不可变的 JSON 输入。每次上传必须具有 JWT 验证的
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
或 secret manager 获得凭据。上传会拒绝可识别的明文私钥和常见 access-token 形式；
元数据不会返回 secret。此 API 不读取或迁移历史 blob 对象。
