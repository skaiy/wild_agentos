# 分离演示租户与平台管理租户

演示令牌通常会被广泛共享，而全局配置会影响所有租户，因此演示访问权限不能等同于平台管理权限。经过验证的令牌携带租户 ID 时，该租户即被隐式使用，无需注册租户。

## 约定

| 用途 | 租户 | 项目 | 使用者 | 角色 |
| --- | --- | --- | --- | --- |
| 演示 | `demo` | `showcase` | `demo-user` | `DA`、`mcp_invoke` |
| 平台管理 | `platform` | 由部署决定 | 受限管理员 | `PLATFORM_ADMIN` |

可以通过 `DEMO_TENANT`、`DEMO_PROJECT` 和 `DEMO_ACTOR` 修改演示名称。绝不能给演示令牌添加 `PLATFORM_ADMIN`。脚本拒绝 `default`、与平台租户重合的租户，以及 `DEMO_ROLES` 中的 `PLATFORM_ADMIN`。

## 前提条件

将 `AGENTOS_JWT_SECRET` 设为部署使用的 HS256 签名密钥（至少 32 字节）。在服务端设置 `AGENTOS_PLATFORM_ADMIN_TENANT=platform`；如果实际平台租户不是 `platform`，请使用实际名称。它不能是 `default` 或演示租户。添加示例文档需要运行中的本地 HTTP API，并启用 embedding 和文档存储。请妥善保管签名密钥和令牌文件。

在平台管理权限检查（#274）**和**配置读取权限检查（#290）都已部署之前，不要给线上演示令牌添加 `DA`。旧版本内核的配置读取检查可能显示跳过，这不表示可以上线。

## 签发、添加示例、验证

```sh
# 在私有环境中设置 AGENTOS_JWT_SECRET，不要将其粘贴到命令中。
export AGENTOS_PLATFORM_ADMIN_TENANT=platform
scripts/demo-tenant.sh mint --out ./demo-tenant.jwt --exp-days 7
DEMO_TOKEN_FILE=./demo-tenant.jwt BASE_URL=http://127.0.0.1:8080 scripts/demo-tenant.sh seed
DEMO_TOKEN_FILE=./demo-tenant.jwt BASE_URL=http://127.0.0.1:8080 scripts/demo-tenant.sh verify
```

`mint` 创建权限为 600 的令牌文件，仅输出路径与声明摘要。`seed` 按名称跳过已有的演示智能体、知识库和文档，并从 `scripts/examples/demo/` 上传虚构的示例内容。`verify` 检查演示数据读取、全局配置写入拒绝，以及另一租户无法看到或更新演示智能体。不要提交或传播令牌文件。

## 轮换与现有数据

重新运行 `mint` 并指定新有效期即可替换文件。轮换 `AGENTOS_JWT_SECRET` 会撤销使用旧密钥签发的**所有**令牌，而不只是演示令牌。`default` 下已有的演示数据**不会**自动迁移；迁移前请查看 `scripts/isolation-migrate`。
