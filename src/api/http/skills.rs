//! 技能注册 / manifest / Git 导入 / 准入流水线。
//!
//! 路由仍由 `mod.rs` 的 `build_router` 组装；本模块承载持久化、处理器与技能相关测试。

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tools::skill_registry::SkillMeta;

use super::iam::UserIdentity;
use super::{data_dir, AppState};

/// 用户态注册技能的持久化文件路径（仅 POST 注册的技能，不含启动播种的默认技能）。
fn skills_store_path() -> std::path::PathBuf {
    data_dir().join("skills.json")
}

/// 启动时从磁盘加载用户态注册的技能；文件不存在或解析失败时返回空列表。
pub(crate) fn load_user_skills() -> Vec<SkillMeta> {
    match std::fs::read_to_string(skills_store_path()) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 以 skill_iri 为主键 upsert 一条用户态技能并持久化（pretty JSON）。
fn save_user_skill(skill: &SkillMeta) -> std::io::Result<()> {
    let mut skills = load_user_skills();
    match skills.iter_mut().find(|s| s.skill_iri == skill.skill_iri) {
        Some(existing) => *existing = skill.clone(),
        None => skills.push(skill.clone()),
    }
    let path = skills_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(&skills).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}

/// 按 skill_iri 从用户态技能文件删除一条并持久化。返回是否原本存在。
fn delete_user_skill(skill_iri: &str) -> std::io::Result<bool> {
    let mut skills = load_user_skills();
    let before = skills.len();
    skills.retain(|s| s.skill_iri != skill_iri);
    let existed = skills.len() != before;
    if existed {
        let path = skills_store_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&skills).unwrap_or_else(|_| "[]".to_string());
        std::fs::write(&path, content)?;
    }
    Ok(existed)
}

/// 判定是否为系统级内置技能（`iri://` 命名空间，由内核启动播种，只读）。
fn is_system_skill_iri(iri: &str) -> bool {
    iri.starts_with("iri://")
}

/// 技能准入流水线运行记录的持久化文件路径。
pub(crate) fn pipeline_runs_path() -> std::path::PathBuf {
    data_dir().join("pipeline_runs.json")
}

/// 保留的最近流水线运行记录条数上限（超出则裁剪最早记录）。
const PIPELINE_RUNS_CAP: usize = 200;

/// 从磁盘加载流水线运行记录（最新在前）；文件不存在或解析失败时返回空列表。
fn load_pipeline_runs() -> Vec<crate::tools::skill_pipeline::PipelineRun> {
    match std::fs::read_to_string(pipeline_runs_path()) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 追加一条运行记录并持久化（最新在前，超上限裁剪最早）。best-effort。
fn append_pipeline_run(run: &crate::tools::skill_pipeline::PipelineRun) -> std::io::Result<()> {
    let mut runs = load_pipeline_runs();
    runs.insert(0, run.clone());
    if runs.len() > PIPELINE_RUNS_CAP {
        runs.truncate(PIPELINE_RUNS_CAP);
    }
    let path = pipeline_runs_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(&runs).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}

pub(crate) async fn list_skills_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let skills = state.core.skills.list_all_skills();
    let trusted_key_count = state.core.skills.trusted_key_count();
    let enriched: Vec<Value> = skills
        .iter()
        .map(|s| {
            let status = state.core.skills.verify_skill_signature(s);
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("signature_status".into(), json!(status.as_str()));
            }
            v
        })
        .collect();
    Json(json!({
        "count": enriched.len(),
        "trusted_key_count": trusted_key_count,
        "skills": enriched,
    }))
}

/// skill.yaml 下载端点的查询参数。
#[derive(Deserialize)]
pub(crate) struct SkillManifestQuery {
    iri: String,
}

/// 将字符串转义为合法的 YAML 双引号标量。
pub(crate) fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 依据已注册的技能元数据生成标准化 skill.yaml 文本。
/// input_schema / output_schema 直接内联为 JSON（YAML 是 JSON 的超集，合法）。
pub(crate) fn build_skill_yaml(skill: &SkillMeta, signature_status: &str) -> String {
    let roles_json = serde_json::to_string(&skill.allowed_roles).unwrap_or_else(|_| "[]".into());
    let perms_json = serde_json::to_string(&skill.skill_types).unwrap_or_else(|_| "[]".into());
    let input_json = serde_json::to_string(&skill.input_schema).unwrap_or_else(|_| "{}".into());
    let output_json = serde_json::to_string(&skill.output_schema).unwrap_or_else(|_| "{}".into());
    format!(
        "# skill.yaml — 由 Wild AgentOS 依据已注册技能元数据生成\n\
apiVersion: agentos.dev/v1\n\
kind: Skill\n\
metadata:\n\
\x20 iri: {iri}\n\
\x20 name: {name}\n\
\x20 version: {version}\n\
\x20 category: {category}\n\
spec:\n\
\x20 description: {desc}\n\
\x20 security_level: {sec}\n\
\x20 signature_status: {sig}\n\
\x20 allowed_roles: {roles}\n\
\x20 permissions: {perms}\n\
\x20 input_schema: {input}\n\
\x20 output_schema: {output}\n",
        iri = yaml_quote(&skill.skill_iri),
        name = yaml_quote(&skill.name),
        version = yaml_quote(&skill.version),
        category = yaml_quote(&skill.category),
        desc = yaml_quote(&skill.description),
        sec = yaml_quote(&skill.security_level),
        sig = yaml_quote(signature_status),
        roles = roles_json,
        perms = perms_json,
        input = input_json,
        output = output_json,
    )
}

/// GET /api/v1/skills/manifest?iri=... — 生成并下载指定技能的 skill.yaml。
pub(crate) async fn skill_manifest_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SkillManifestQuery>,
) -> impl IntoResponse {
    match state.core.skills.get_skill(&q.iri) {
        Some(skill) => {
            let sig = state.core.skills.verify_skill_signature(&skill);
            let yaml = build_skill_yaml(&skill, sig.as_str());
            // 文件名以技能名为基，去除路径分隔符等不安全字符。
            let safe: String = skill
                .name
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let filename = if safe.is_empty() {
                "skill".to_string()
            } else {
                safe
            };
            (
                StatusCode::OK,
                [
                    (
                        header::CONTENT_TYPE,
                        "application/x-yaml; charset=utf-8".to_string(),
                    ),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{}.skill.yaml\"", filename),
                    ),
                ],
                yaml,
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "status": "error", "error": "技能不存在", "iri": q.iri })),
        )
            .into_response(),
    }
}

/// POST /api/v1/skills — 注册新技能（G7：仅 DA 角色可用）
pub(crate) async fn register_skill_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(skill): Json<SkillMeta>,
) -> impl IntoResponse {
    // G7：严格模式下要求 DA 角色
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    if skill.skill_iri.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error", "error": "skill_iri 不能为空"
            })),
        )
            .into_response();
    }
    // 系统级命名空间（iri://）保留给内核内置技能，只读——不可经 API 注册或覆盖。
    if is_system_skill_iri(&skill.skill_iri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "status": "error",
                "error": "系统级内置技能（iri://）为只读，不可注册或修改；请使用 skill:// 命名空间",
            })),
        )
            .into_response();
    }
    // 走技能准入流水线（Lint→Security→Test→Publish）：签名/Schema 等门禁在流水线内统一裁决，
    // 仅当门禁放行时 publish 回调才会真正持久化并注册技能。
    use crate::tools::skill_pipeline::{run_pipeline, PipelineContext, PipelineSource};
    let iri = skill.skill_iri.clone();
    let ctx = PipelineContext::local(PipelineSource::Manual, identity.user_id.clone());
    let registry = state.core.skills.clone();
    let run = run_pipeline(
        &state.core.skills,
        &skill,
        &ctx,
        Box::new(move |s| {
            save_user_skill(s).map_err(|e| e.to_string())?;
            registry.register_skill(s.clone());
            Ok(format!("已注册并持久化技能 {}", s.skill_iri))
        }),
    );
    let _ = append_pipeline_run(&run);

    let sig_status = state.core.skills.verify_skill_signature(&skill);
    let code = if run.published {
        StatusCode::CREATED
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    (
        code,
        Json(json!({
            "status": if run.published { "ok" } else { "error" },
            "error": if run.published { Value::Null } else { json!(run.summary) },
            "skill_iri": iri,
            "signature_status": sig_status.as_str(),
            "gate_passed": run.gate_passed,
            "published": run.published,
            "registered_by": identity.user_id,
            "tenant_id": identity.tenant_id,
            "pipeline_run": run,
        })),
    )
        .into_response()
}

/// DELETE /api/v1/skills?iri=... — 删除应用级技能（G7：仅 DA 角色）。
/// 系统级内置技能（iri://）只读，拒绝删除。
pub(crate) async fn delete_skill_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Query(q): Query<SkillManifestQuery>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    if q.iri.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error", "error": "iri 不能为空"
            })),
        )
            .into_response();
    }
    if is_system_skill_iri(&q.iri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "status": "error",
                "error": "系统级内置技能（iri://）为只读，不可删除",
            })),
        )
            .into_response();
    }
    let removed_mem = state.core.skills.remove_skill(&q.iri);
    let removed_disk = delete_user_skill(&q.iri).unwrap_or(false);
    if !removed_mem && !removed_disk {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "status": "error", "error": "技能未找到（检查 iri）"
            })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "skill_iri": q.iri,
            "deleted_by": identity.user_id,
        })),
    )
        .into_response()
}

// ──────────────────────────────────────────────────────────────────────────────
// Git 技能导入
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct GitImportRequest {
    /// Git 仓库 URL（https:// 或 git@）。
    repo_url: String,
    /// 分支/Tag/Commit，缺省 "main"。
    #[serde(default = "default_ref")]
    r#ref: String,
    /// 仓库内 skill.yaml 所在子目录，缺省根目录 "."。
    #[serde(default = "default_path")]
    path: String,
    // ── 下列字段为可选覆盖（优先于 skill.yaml 中同名字段） ──
    skill_iri: Option<String>,
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    category: Option<String>,
    security_level: Option<String>,
    allowed_roles: Option<Vec<String>>,
    skill_types: Option<Vec<String>>,
}

fn default_ref() -> String {
    "main".into()
}
pub(crate) fn normalize_git_skill_subpath(path: &str) -> Result<std::path::PathBuf, &'static str> {
    let requested_path = path.trim();
    if requested_path.is_empty() || requested_path == "." || requested_path == "/" {
        return Ok(std::path::PathBuf::new());
    }
    let path = std::path::Path::new(requested_path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err("技能目录路径必须是仓库内的相对路径，且不能包含 ..");
    }
    Ok(path.to_path_buf())
}

fn default_path() -> String {
    ".".into()
}

/// 从 skill.yaml 文本中解析扁平化 key→value 映射（支持 metadata/spec 两级）。
/// 不依赖任何外部 YAML 库，直接按行分析。
pub(crate) fn parse_skill_yaml_text(yaml: &str) -> HashMap<String, String> {
    let mut flat: HashMap<String, String> = HashMap::new();
    let mut section = String::new();
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if let Some(colon_pos) = trimmed.find(':') {
            let key = trimmed[..colon_pos].trim().to_string();
            let value_raw = trimmed[colon_pos + 1..].trim().to_string();
            if indent == 0 {
                if value_raw.is_empty() {
                    section = key;
                } else {
                    flat.insert(key, yaml_unquote(&value_raw));
                }
            } else {
                let full_key = if section.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", section, key)
                };
                if !value_raw.is_empty() {
                    flat.insert(full_key, yaml_unquote(&value_raw));
                }
            }
        }
    }
    flat
}

fn yaml_unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].replace("\\\"", "\"").replace("\\'", "'")
    } else {
        s.to_string()
    }
}

/// 从 Git 仓库 URL 派生默认 skill IRI。
/// 例：https://github.com/org/repo.git → skill://org/repo
pub(crate) fn iri_from_git_url(url: &str) -> String {
    let base = url.trim_end_matches(".git");
    let without_proto: String = if let Some(rest) = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
    {
        rest.to_string()
    } else if let Some(rest) = base.strip_prefix("git@") {
        rest.replacen(':', "/", 1)
    } else {
        base.to_string()
    };
    let parts: Vec<&str> = without_proto.trim_matches('/').split('/').collect();
    if parts.len() >= 2 {
        format!(
            "skill://{}/{}",
            parts[parts.len() - 2],
            parts[parts.len() - 1]
        )
    } else {
        format!("skill://repo/{}", without_proto.replace('/', "-"))
    }
}

/// POST /api/v1/skills/import-git — 从 Git 仓库导入技能。
/// 需要 DA 角色（X-Identity 头）。
pub(crate) async fn import_git_skill_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<GitImportRequest>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    if req.repo_url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "repo_url 不能为空" })),
        )
            .into_response();
    }
    let relative_path = match normalize_git_skill_subpath(&req.path) {
        Ok(path) => path,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": "error",
                    "error": error,
                })),
            )
                .into_response();
        }
    };

    // 1. git clone --depth 1 -b <ref> <url> /tmp/<uuid>
    let clone_dir = std::env::temp_dir().join(format!("waos-skill-{}", uuid::Uuid::new_v4()));
    let mut output = match tokio::process::Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "-b",
            req.r#ref.as_str(),
            "--single-branch",
            req.repo_url.trim(),
            clone_dir.to_str().unwrap_or("/tmp/waos-skill-clone"),
        ])
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "error": format!("无法执行 git clone：{error}"),
                })),
            )
                .into_response();
        }
    };

    // cleanup helper (best-effort; ignore errors)
    let cleanup = |dir: &std::path::Path| {
        let _ = std::fs::remove_dir_all(dir);
    };

    if !output.status.success() && req.r#ref == "main" {
        cleanup(&clone_dir);
        output = match tokio::process::Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                req.repo_url.as_str(),
                clone_dir.to_string_lossy().as_ref(),
            ])
            .output()
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "status": "error",
                        "error": format!("无法执行 git clone：{error}"),
                    })),
                )
                    .into_response();
            }
        };
    }
    if !output.status.success() {
        cleanup(&clone_dir);
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "status": "error",
                "error": "Git 仓库克隆失败，请检查仓库地址、分支和访问权限",
            })),
        )
            .into_response();
    }

    // 2. 读取并校验仓库内 skill.yaml，不允许路径越界或静默回退为占位技能。
    let clone_root = match std::fs::canonicalize(&clone_dir) {
        Ok(path) => path,
        Err(error) => {
            cleanup(&clone_dir);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "error": format!("无法读取克隆目录：{error}"),
                })),
            )
                .into_response();
        }
    };
    let skill_yaml_path = clone_dir.join(relative_path).join("skill.yaml");
    let canonical_yaml = match std::fs::canonicalize(&skill_yaml_path) {
        Ok(path) if path.starts_with(&clone_root) => path,
        _ => {
            cleanup(&clone_dir);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": "error",
                    "error": "指定目录中未找到 skill.yaml",
                })),
            )
                .into_response();
        }
    };
    let yaml_text = match std::fs::read_to_string(&canonical_yaml) {
        Ok(text) => text,
        Err(error) => {
            cleanup(&clone_dir);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "status": "error",
                    "error": format!("skill.yaml 读取失败：{error}"),
                })),
            )
                .into_response();
        }
    };
    let yaml_fields = parse_skill_yaml_text(&yaml_text);

    // 3. 合并字段（请求体优先）。
    let skill_iri = req
        .skill_iri
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("metadata.iri").cloned())
        .unwrap_or_else(|| iri_from_git_url(&req.repo_url));

    let name = req
        .name
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("metadata.name").cloned())
        .unwrap_or_else(|| {
            skill_iri
                .split('/')
                .next_back()
                .unwrap_or("unnamed")
                .to_string()
        });

    let description = req
        .description
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("spec.description").cloned())
        .unwrap_or_default();

    let version = req
        .version
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("metadata.version").cloned())
        .unwrap_or_else(|| "1.0.0".into());

    let category = req
        .category
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("metadata.category").cloned())
        .unwrap_or_else(|| "application".into());

    let security_level = req
        .security_level
        .filter(|s| !s.is_empty())
        .or_else(|| yaml_fields.get("spec.security_level").cloned())
        .unwrap_or_else(|| "normal".into());

    let allowed_roles = req.allowed_roles.unwrap_or_else(|| {
        // 尝试从 yaml 字段解析 JSON 数组
        yaml_fields
            .get("spec.allowed_roles")
            .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
            .unwrap_or_else(|| vec!["DA".into()])
    });

    let skill_types = req.skill_types.unwrap_or_else(|| {
        yaml_fields
            .get("spec.permissions")
            .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
            .unwrap_or_default()
    });

    if skill_iri.is_empty() {
        cleanup(&clone_dir);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "error": "无法确定 skill_iri，请手动填写" })),
        )
            .into_response();
    }

    let skill = SkillMeta {
        skill_iri: skill_iri.clone(),
        name: name.clone(),
        description,
        version,
        category,
        security_level,
        allowed_roles,
        input_schema: serde_json::Value::Object(Default::default()),
        output_schema: serde_json::Value::Object(Default::default()),
        compiled_template: String::new(),
        signature: None,
        signature_algorithm: None,
        input_mapping: HashMap::new(),
        output_mapping: HashMap::new(),
        skill_types,
    };

    // 走技能准入流水线（Git 来源）：Security/Test 阶段会对克隆目录做敏感扫描与示例夹具校验，
    // 因此流水线必须在 cleanup 清理克隆目录之前执行。
    use crate::tools::skill_pipeline::{run_pipeline, PipelineContext, PipelineSource};
    let ctx = PipelineContext {
        source: PipelineSource::Git,
        triggered_by: identity.user_id.clone(),
        repo_url: Some(req.repo_url.trim().to_string()),
        clone_dir: Some(clone_dir.clone()),
        sub_path: req.path.clone(),
    };
    let registry = state.core.skills.clone();
    let run = run_pipeline(
        &state.core.skills,
        &skill,
        &ctx,
        Box::new(move |s| {
            save_user_skill(s).map_err(|e| e.to_string())?;
            registry.register_skill(s.clone());
            Ok(format!("已注册并持久化技能 {}", s.skill_iri))
        }),
    );
    let _ = append_pipeline_run(&run);

    let sig_status = state.core.skills.verify_skill_signature(&skill);
    cleanup(&clone_dir);

    let code = if run.published {
        StatusCode::CREATED
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    (
        code,
        Json(json!({
            "status": if run.published { "ok" } else { "error" },
            "error": if run.published { Value::Null } else { json!(run.summary) },
            "skill_iri": skill_iri,
            "name": name,
            "git_cloned": true,
            "git_stderr": "",
            "yaml_fields_found": yaml_fields.len(),
            "signature_status": sig_status.as_str(),
            "gate_passed": run.gate_passed,
            "published": run.published,
            "registered_by": identity.user_id,
            "pipeline_run": run,
        })),
    )
        .into_response()
}

// ──────────────────────────────────────────────────────────────────────────────
// 技能准入流水线：运行记录查询 + 重跑
// ──────────────────────────────────────────────────────────────────────────────

/// GET /api/v1/skills/pipeline-runs 的查询参数。
#[derive(Debug, Deserialize)]
pub(crate) struct PipelineRunsQuery {
    /// 可选：按 skill_iri 过滤（详情弹窗按单个技能拉取其历史）。
    iri: Option<String>,
    /// 可选：仅返回最近 N 条（默认全部，受服务端上限约束）。
    limit: Option<usize>,
}

/// GET /api/v1/skills/pipeline-runs — 查询技能准入流水线运行记录（只读，无需鉴权）。
/// 记录仅含文件名/命中计数等非敏感信息，可安全对管理台展示。
pub(crate) async fn list_pipeline_runs_handler(Query(q): Query<PipelineRunsQuery>) -> impl IntoResponse {
    let mut runs = load_pipeline_runs();
    if let Some(iri) = q.iri.filter(|s| !s.is_empty()) {
        runs.retain(|r| r.skill_iri == iri);
    }
    if let Some(limit) = q.limit {
        runs.truncate(limit);
    }
    Json(json!({
        "count": runs.len(),
        "runs": runs,
    }))
}

/// POST /api/v1/skills/pipeline-rerun 的请求体。
#[derive(Debug, Deserialize)]
pub(crate) struct PipelineRerunRequest {
    skill_iri: String,
}

/// POST /api/v1/skills/pipeline-rerun — 对已注册的应用级技能重跑准入流水线（G7：仅 DA 角色）。
pub(crate) async fn pipeline_rerun_handler(
    State(state): State<Arc<AppState>>,
    identity: UserIdentity,
    Json(req): Json<PipelineRerunRequest>,
) -> impl IntoResponse {
    if let Err(e) = identity.require_role("DA") {
        return e.into_response();
    }
    if req.skill_iri.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error", "error": "skill_iri 不能为空"
            })),
        )
            .into_response();
    }
    if is_system_skill_iri(&req.skill_iri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "status": "error", "error": "系统级内置技能（iri://）为只读，不支持重跑流水线",
            })),
        )
            .into_response();
    }
    // 以持久化的用户态技能为准（含完整 schema/template/签名）。
    let skill = match load_user_skills()
        .into_iter()
        .find(|s| s.skill_iri == req.skill_iri)
    {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "status": "error", "error": "技能未找到（检查 skill_iri）"
                })),
            )
                .into_response();
        }
    };

    use crate::tools::skill_pipeline::{run_pipeline, PipelineContext, PipelineSource};
    let ctx = PipelineContext::local(PipelineSource::Rerun, identity.user_id.clone());
    let registry = state.core.skills.clone();
    let run = run_pipeline(
        &state.core.skills,
        &skill,
        &ctx,
        Box::new(move |s| {
            save_user_skill(s).map_err(|e| e.to_string())?;
            registry.register_skill(s.clone());
            Ok(format!("已重新注册技能 {}", s.skill_iri))
        }),
    );
    let _ = append_pipeline_run(&run);

    (
        StatusCode::OK,
        Json(json!({
            "status": if run.published { "ok" } else { "error" },
            "error": if run.published { Value::Null } else { json!(run.summary) },
            "skill_iri": req.skill_iri,
            "gate_passed": run.gate_passed,
            "published": run.published,
            "pipeline_run": run,
        })),
    )
        .into_response()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::core_types::{CoreConfig, SemanticCore};
    use crate::tools::prompt_registry::PromptRegistry;
    use axum::{
        routing::{get, post},
        Router,
    };
    use axum::http::StatusCode;
    use base64::Engine;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::super::{api_gov::ApiUsageState, TEST_ENV_LOCK};

    // ── 辅助：最小 AppState ───────────────────────────────────────────────────
    fn make_state(tmp: &std::path::Path) -> Arc<AppState> {
        let l0 = tmp.join("l0");
        std::fs::create_dir_all(&l0).unwrap();
        let core = Arc::new(
            SemanticCore::new(CoreConfig {
                max_node_size: 1024,
                max_projection_size: 2048,
                l0_storage_path: l0.to_str().unwrap().to_string(),
                event_buffer_size: 10,
                enable_metrics: false,
                eviction_config: None,
            })
            .unwrap(),
        );
        let gateway = Arc::new(
            crate::gateway::UnifiedGateway::new(&crate::config::GatewaySettings {
                base_url: "http://localhost".into(),
                api_key: String::new(),
                default_model: "test-model".into(),
                timeout_seconds: 30,
                max_retries: 1,
                retry_base_ms: 500,
                use_responses_api: false,
                model_mapping: std::collections::HashMap::new(),
            })
            .unwrap(),
        );
        let kg_store = Arc::new(oxigraph::store::Store::new().unwrap());
        Arc::new(AppState {
            core,
            gateway,
            kg_store,
            config_info: Arc::new(tokio::sync::RwLock::new(serde_json::json!({}))),
            agents_info: serde_json::json!({ "count": 0, "agents": [] }),
            mcp_servers: Arc::new(tokio::sync::RwLock::new(vec![])),
            user_agents: Arc::new(tokio::sync::RwLock::new(vec![])),
            prompts: Arc::new(PromptRegistry::new()),
            kb_categories: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_bases: Arc::new(tokio::sync::RwLock::new(vec![])),
            knowledge_packs: Arc::new(tokio::sync::RwLock::new(vec![])),
            vector_store: Arc::new(arc_swap::ArcSwapOption::empty()),
            blob_store: None,
            task_executor: None,
            batch_manager: None,
            api_clients: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_keys: Arc::new(tokio::sync::RwLock::new(vec![])),
            api_usage: Arc::new(ApiUsageState::default()),
        })
    }

    fn sample_skill() -> SkillMeta {
        SkillMeta {
            skill_iri: "skill://test/hello".into(),
            name: "Hello World".into(),
            description: "测试技能".into(),
            version: "1.0.0".into(),
            category: "test".into(),
            security_level: "standard".into(),
            allowed_roles: vec!["DA".into()],
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: serde_json::json!({"type": "object"}),
            compiled_template: "{{x}}".into(),
            signature: None,
            signature_algorithm: None,
            input_mapping: Default::default(),
            output_mapping: Default::default(),
            skill_types: vec![],
        }
    }
    // ── 纯函数单元测试 ────────────────────────────────────────────────────────

    #[test]
    fn test_yaml_quote_plain() {
        assert_eq!(yaml_quote("hello"), "\"hello\"");
    }

    #[test]
    fn test_yaml_quote_with_quotes() {
        // 双引号与反斜杠应被转义
        assert_eq!(yaml_quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(yaml_quote(r"back\slash"), r#""back\\slash""#);
    }

    #[test]
    fn test_build_skill_yaml_contains_fields() {
        let skill = sample_skill();
        let yaml = build_skill_yaml(&skill, "unsigned");
        assert!(yaml.contains("skill://test/hello"), "should contain IRI");
        assert!(yaml.contains("Hello World"), "should contain name");
        assert!(yaml.contains("1.0.0"), "should contain version");
        assert!(yaml.contains("unsigned"), "should contain signature_status");
        assert!(
            yaml.contains("allowed_roles:"),
            "should contain allowed_roles key"
        );
        assert!(yaml.contains("DA"), "should contain DA role");
    }

    #[test]
    fn test_iri_from_git_url_https() {
        assert_eq!(
            iri_from_git_url("https://github.com/myorg/myrepo.git"),
            "skill://myorg/myrepo"
        );
    }

    #[test]
    fn test_iri_from_git_url_https_no_git_suffix() {
        assert_eq!(
            iri_from_git_url("https://gitee.com/acme/demo-skill"),
            "skill://acme/demo-skill"
        );
    }

    #[test]
    fn test_iri_from_git_url_ssh() {
        assert_eq!(
            iri_from_git_url("git@github.com:myorg/myrepo.git"),
            "skill://myorg/myrepo"
        );
    }

    #[test]
    fn test_parse_skill_yaml_text_flat() {
        let yaml = "\
skill_iri: \"skill://test/demo\"\n\
name: \"演示技能\"\n\
version: \"2.0.0\"\n\
";
        let map = parse_skill_yaml_text(yaml);
        assert_eq!(
            map.get("skill_iri").map(|s| s.as_str()),
            Some("skill://test/demo")
        );
        assert_eq!(map.get("name").map(|s| s.as_str()), Some("演示技能"));
        assert_eq!(map.get("version").map(|s| s.as_str()), Some("2.0.0"));
    }

    #[test]
    fn test_parse_skill_yaml_text_nested() {
        // 两级嵌套（metadata / spec），键应被扁平化为 "section.key"。
        // 注意：用 concat! 保留缩进——字符串行尾 `\` 会连同下一行前导空格一并吞掉。
        let yaml = concat!(
            "metadata:\n",
            "  iri: \"skill://test/nested\"\n",
            "  name: \"嵌套技能\"\n",
            "  version: \"3.1.0\"\n",
            "  category: \"application\"\n",
            "spec:\n",
            "  description: \"支持嵌套解析\"\n",
            "  security_level: \"normal\"\n",
        );
        let map = parse_skill_yaml_text(yaml);
        assert_eq!(
            map.get("metadata.iri").map(|s| s.as_str()),
            Some("skill://test/nested")
        );
        assert_eq!(
            map.get("metadata.name").map(|s| s.as_str()),
            Some("嵌套技能")
        );
        assert_eq!(
            map.get("metadata.version").map(|s| s.as_str()),
            Some("3.1.0")
        );
        assert_eq!(
            map.get("metadata.category").map(|s| s.as_str()),
            Some("application")
        );
        assert_eq!(
            map.get("spec.description").map(|s| s.as_str()),
            Some("支持嵌套解析")
        );
        assert_eq!(
            map.get("spec.security_level").map(|s| s.as_str()),
            Some("normal")
        );
    }

    #[test]
    fn test_normalize_git_skill_subpath_accepts_repository_paths() {
        assert_eq!(
            normalize_git_skill_subpath("/").unwrap(),
            std::path::PathBuf::new()
        );
        assert_eq!(
            normalize_git_skill_subpath("skills/pdf-parser").unwrap(),
            std::path::PathBuf::from("skills/pdf-parser")
        );
    }

    #[test]
    fn test_normalize_git_skill_subpath_rejects_escape_paths() {
        assert!(normalize_git_skill_subpath("../outside").is_err());
        assert!(normalize_git_skill_subpath("skills/../../outside").is_err());
        assert!(normalize_git_skill_subpath("/tmp/outside").is_err());
    }

    // ── HTTP 集成测试 ─────────────────────────────────────────────────────────

    /// GET /api/v1/skills/manifest?iri=skill://test/hello → 200 + application/x-yaml
    #[tokio::test]
    async fn test_manifest_200_known_skill() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("manifest_200_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        state.core.skills.register_skill(sample_skill());

        let router = Router::new()
            .route("/api/v1/skills/manifest", get(skill_manifest_handler))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/v1/skills/manifest?iri=skill://test/hello")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("yaml"),
            "content-type should be yaml, got: {ct}"
        );
        let cd = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            cd.contains("attachment"),
            "should be an attachment download"
        );

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// GET /api/v1/skills/manifest?iri=skill://notfound/x → 404
    #[tokio::test]
    async fn test_manifest_404_unknown_skill() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("manifest_404_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);

        let router = Router::new()
            .route("/api/v1/skills/manifest", get(skill_manifest_handler))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/v1/skills/manifest?iri=skill://notfound/x")
            .body(axum::body::Body::empty())
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// POST /api/v1/skills/import-git 无 X-Identity 头 → 403（严格模式）
    #[tokio::test]
    async fn test_import_git_403_no_role() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("importgit_403_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");

        let state = make_state(&tmp);

        let router = Router::new()
            .route("/api/v1/skills/import-git", post(import_git_skill_handler))
            .with_state(state);

        let body =
            serde_json::json!({ "repo_url": "https://github.com/test/repo.git" }).to_string();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/skills/import-git")
            .header("content-type", "application/json")
            // 故意不带 X-Identity
            .body(axum::body::Body::from(body))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        std::env::remove_var("AGENTOS_DATA_DIR");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// POST /api/v1/skills/import-git 带 DA 角色但 repo_url 为空 → 400
    #[tokio::test]
    async fn test_import_git_400_empty_url() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("importgit_400_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);

        let router = Router::new()
            .route("/api/v1/skills/import-git", post(import_git_skill_handler))
            .with_state(state);

        let identity = STANDARD.encode(
            serde_json::json!({"user_id": "admin", "tenant_id": "t-test", "roles": ["DA"]})
                .to_string(),
        );
        // repo_url 为空字符串
        let body = serde_json::json!({ "repo_url": "" }).to_string();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/skills/import-git")
            .header("content-type", "application/json")
            .header("x-identity", identity)
            .body(axum::body::Body::from(body))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    // ── 技能准入流水线集成测试 ─────────────────────────────────────────────────

    use base64::engine::general_purpose::STANDARD as B64;

    /// 构造带 DA 角色的 X-Identity 头值。
    fn da_identity(user: &str) -> String {
        B64.encode(
            serde_json::json!({"user_id": user, "tenant_id": "t-test", "roles": ["DA"]})
                .to_string(),
        )
    }

    /// POST 一个 JSON body 到 router，返回 (状态码, 解析后的 body)。
    async fn post_json(
        router: &Router,
        uri: &str,
        body: Value,
        ident: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut b = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(id) = ident {
            b = b.header("x-identity", id);
        }
        let req = b.body(axum::body::Body::from(body.to_string())).unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    /// GET 一个 URI，返回 (状态码, 解析后的 body)。
    async fn get_json(router: &Router, uri: &str) -> (StatusCode, Value) {
        let req = axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    /// 合法技能注册 → 201 CREATED，门禁放行且已发布，运行记录持久化并可经查询接口检索。
    #[tokio::test]
    async fn test_pipeline_manual_register_ok_persists_run() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pipeline_ok_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        let router = Router::new()
            .route("/api/v1/skills", post(register_skill_handler))
            .route(
                "/api/v1/skills/pipeline-runs",
                get(list_pipeline_runs_handler),
            )
            .with_state(state);

        let ident = da_identity("admin");
        let skill = serde_json::json!({
            "skill_iri": "skill://test/ok", "name": "合法技能", "description": "有效",
            "version": "1.0.0", "category": "test", "security_level": "standard",
            "allowed_roles": ["DA"], "input_schema": {"type": "object"},
            "output_schema": {"type": "object"}, "compiled_template": "{{x}}"
        });
        let (st, body) = post_json(&router, "/api/v1/skills", skill, Some(&ident)).await;
        assert_eq!(st, StatusCode::CREATED, "合法技能应 201，body={body}");
        assert_eq!(body["gate_passed"], true);
        assert_eq!(body["published"], true);
        assert_eq!(body["pipeline_run"]["source"], "manual");
        assert_eq!(body["pipeline_run"]["skill_iri"], "skill://test/ok");

        // 运行记录已持久化落盘。
        let disk = std::fs::read_to_string(pipeline_runs_path()).unwrap();
        assert!(disk.contains("skill://test/ok"), "运行记录应落盘");

        // 查询接口（按 iri 过滤）应可检索到该运行。
        let (st, listed) =
            get_json(&router, "/api/v1/skills/pipeline-runs?iri=skill://test/ok").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["runs"][0]["published"], true);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 非法 input_schema（无法编译为 JSON Schema）→ 422，门禁拦截、未发布，
    /// 但失败运行记录仍持久化且可查询。
    #[tokio::test]
    async fn test_pipeline_manual_register_invalid_schema_422() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pipeline_422_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        let router = Router::new()
            .route("/api/v1/skills", post(register_skill_handler))
            .route(
                "/api/v1/skills/pipeline-runs",
                get(list_pipeline_runs_handler),
            )
            .with_state(state.clone());

        let ident = da_identity("admin");
        // type 必须是字符串/数组；此处为数字 → JSON Schema 编译失败 → Lint 阶段 Failed。
        let skill = serde_json::json!({
            "skill_iri": "skill://test/bad", "name": "非法技能", "description": "无效",
            "version": "1.0.0", "category": "test", "security_level": "standard",
            "allowed_roles": ["DA"], "input_schema": {"type": 123},
            "output_schema": {"type": "object"}, "compiled_template": "{{x}}"
        });
        let (st, body) = post_json(&router, "/api/v1/skills", skill, Some(&ident)).await;
        assert_eq!(
            st,
            StatusCode::UNPROCESSABLE_ENTITY,
            "非法 schema 应 422，body={body}"
        );
        assert_eq!(body["gate_passed"], false);
        assert_eq!(body["published"], false);

        // 技能不得被真正注册。
        assert!(
            state.core.skills.get_skill("skill://test/bad").is_none(),
            "门禁拦截后不应注册"
        );

        // 失败运行记录仍持久化，且门禁字段为 false。
        let (st, listed) =
            get_json(&router, "/api/v1/skills/pipeline-runs?iri=skill://test/bad").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["runs"][0]["gate_passed"], false);
        assert_eq!(listed["runs"][0]["published"], false);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 对已注册技能重跑流水线 → 200，来源为 rerun，且新增一条运行记录。
    #[tokio::test]
    async fn test_pipeline_rerun_ok() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pipeline_rerun_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        let router = Router::new()
            .route("/api/v1/skills", post(register_skill_handler))
            .route(
                "/api/v1/skills/pipeline-rerun",
                post(pipeline_rerun_handler),
            )
            .route(
                "/api/v1/skills/pipeline-runs",
                get(list_pipeline_runs_handler),
            )
            .with_state(state);

        let ident = da_identity("admin");
        let skill = serde_json::json!({
            "skill_iri": "skill://test/rerun", "name": "可重跑技能", "description": "有效",
            "version": "1.0.0", "category": "test", "security_level": "standard",
            "allowed_roles": ["DA"], "input_schema": {"type": "object"},
            "output_schema": {"type": "object"}, "compiled_template": "{{x}}"
        });
        let (st, _) = post_json(&router, "/api/v1/skills", skill, Some(&ident)).await;
        assert_eq!(st, StatusCode::CREATED);

        // 重跑。
        let (st, body) = post_json(
            &router,
            "/api/v1/skills/pipeline-rerun",
            serde_json::json!({"skill_iri": "skill://test/rerun"}),
            Some(&ident),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "重跑应 200，body={body}");
        assert_eq!(body["published"], true);
        assert_eq!(body["pipeline_run"]["source"], "rerun");

        // 两条运行记录（注册 + 重跑）。
        let (st, listed) = get_json(
            &router,
            "/api/v1/skills/pipeline-runs?iri=skill://test/rerun",
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(listed["count"], 2);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 重跑不存在的技能 → 404。
    #[tokio::test]
    async fn test_pipeline_rerun_not_found_404() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pipeline_rerun404_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AGENTOS_DATA_DIR", &tmp);

        let state = make_state(&tmp);
        let router = Router::new()
            .route(
                "/api/v1/skills/pipeline-rerun",
                post(pipeline_rerun_handler),
            )
            .with_state(state);

        let ident = da_identity("admin");
        let (st, _) = post_json(
            &router,
            "/api/v1/skills/pipeline-rerun",
            serde_json::json!({"skill_iri": "skill://test/nope"}),
            Some(&ident),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        std::env::remove_var("AGENTOS_DATA_DIR");
        let _ = std::fs::remove_dir_all(tmp);
    }

}
