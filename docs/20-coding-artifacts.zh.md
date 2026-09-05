> *本文是 [20-coding-artifacts.md](20-coding-artifacts.md) 的中文翻译。*

# 20. Claims 作用域 Coding 制品

Coding agent 的可重放产物经 claims-scoped artifact API 存储，支持 `patch`、
`run_transcript` 与 `reproduce_script`。每次上传必须具有 JWT 验证的
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
或 secret manager 获得凭据。上传会拒绝可识别的明文私钥和常见 access-token 形式；
元数据不会返回 secret。此 API 不读取或迁移历史 blob 对象。
