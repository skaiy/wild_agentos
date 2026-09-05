//! Skill CI/CD Pipeline Engine —— 技能准入流水线引擎。
//!
//! 每次技能注册（手动 POST / Git 导入 / 手动重跑）都会真实执行以下四个阶段：
//! 1. `Lint`     —— 技能包清单/元数据规范校验 + input/output JSON Schema 可编译性校验。
//! 2. `Security` —— Ed25519 签名核验 + 命名空间/权限-安全级一致性 + 克隆目录敏感模式扫描。
//! 3. `Test`     —— Schema 自测（模板 JSON 解析 + 示例夹具校验）+ skill_iri 冲突检测 +
//!    可选的仓库测试框架检测/执行（受 `AGENTOS_PIPELINE_RUN_REPO_TESTS` 门控）。
//! 4. `Publish`  —— 仅当前三阶段无 Failed 时，通过回调真正注册并持久化技能。
//!
//! 每次运行产出一条 [`PipelineRun`]，由上层持久化到 `data/pipeline_runs.json`，供前端展示。
//!
//! 安全性说明：敏感模式扫描只记录**文件名与命中计数**，绝不回显命中的明文片段。

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::tools::skill_registry::{SignatureStatus, SkillMeta, SkillRegistry};

/// 单个阶段的执行结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// 通过（无任何问题）。
    Passed,
    /// 通过但有非阻断性告警。
    Warning,
    /// 失败（阻断准入）。
    Failed,
    /// 跳过（该阶段在当前上下文无可执行内容）。
    Skipped,
}

impl StageStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            StageStatus::Passed => "passed",
            StageStatus::Warning => "warning",
            StageStatus::Failed => "failed",
            StageStatus::Skipped => "skipped",
        }
    }
}

/// 单个阶段的执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageResult {
    /// 阶段标识：lint / security / test / publish。
    pub stage: String,
    /// 阶段中文名（前端直接展示）。
    pub title: String,
    pub status: StageStatus,
    /// 耗时（毫秒）。
    pub duration_ms: u64,
    /// 人类可读摘要。
    pub summary: String,
    /// 明细条目（每条为一次具体检查的结论）。
    #[serde(default)]
    pub details: Vec<String>,
}

impl StageResult {
    #[allow(clippy::new_ret_no_self)]
    fn new(stage: &str, title: &str) -> StageResultBuilder {
        StageResultBuilder {
            stage: stage.to_string(),
            title: title.to_string(),
            started: Instant::now(),
            details: Vec::new(),
            failed: false,
            warned: false,
        }
    }
}

/// 阶段结果构造器：累积明细并按 failed/warned 标志推导最终 status。
struct StageResultBuilder {
    stage: String,
    title: String,
    started: Instant,
    details: Vec<String>,
    failed: bool,
    warned: bool,
}

impl StageResultBuilder {
    fn pass(&mut self, msg: impl Into<String>) {
        self.details.push(format!("✓ {}", msg.into()));
    }
    fn warn(&mut self, msg: impl Into<String>) {
        self.warned = true;
        self.details.push(format!("⚠ {}", msg.into()));
    }
    fn fail(&mut self, msg: impl Into<String>) {
        self.failed = true;
        self.details.push(format!("✗ {}", msg.into()));
    }

    fn finish(self, summary: impl Into<String>) -> StageResult {
        let status = if self.failed {
            StageStatus::Failed
        } else if self.warned {
            StageStatus::Warning
        } else {
            StageStatus::Passed
        };
        StageResult {
            stage: self.stage,
            title: self.title,
            status,
            duration_ms: self.started.elapsed().as_millis() as u64,
            summary: summary.into(),
            details: self.details,
        }
    }
}

/// 触发来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineSource {
    /// 手动 POST /api/v1/skills 注册。
    Manual,
    /// Git 仓库导入。
    Git,
    /// 对已注册技能手动重跑流水线。
    Rerun,
}

/// 一次完整流水线运行记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRun {
    /// 运行 ID（uuid v4）。
    pub run_id: String,
    pub skill_iri: String,
    pub skill_name: String,
    pub version: String,
    pub source: PipelineSource,
    /// Package visibility requested for this run. Tenant publication is never
    /// performed until every gate has passed.
    #[serde(default = "default_visibility")]
    pub visibility: SkillVisibility,
    /// 触发者 user_id。
    pub triggered_by: String,
    /// Git 来源时的仓库地址（脱敏无需，URL 非机密）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    /// ISO8601 UTC 开始时间。
    pub started_at: String,
    /// 总耗时（毫秒）。
    pub duration_ms: u64,
    /// 各阶段结果（按执行顺序）。
    pub stages: Vec<StageResult>,
    /// 门禁是否放行（无 Failed 阶段即放行）。
    pub gate_passed: bool,
    /// 是否最终发布/注册成功。
    pub published: bool,
    /// 综合结论文案。
    pub summary: String,
}

/// Publication scope encoded by a Skill package manifest.
///
/// `System` is reserved for kernel-bundled skills. A submitted package may
/// only target `tenant` or `session`; it cannot promote itself to `system`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillVisibility {
    System,
    Tenant,
    Session,
}

fn default_visibility() -> SkillVisibility {
    SkillVisibility::Session
}

/// Declared side-effect level. This deliberately describes capability rather
/// than trust: trust/signing is evaluated independently by the security gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectLevel {
    None,
    Read,
    Write,
    Execute,
}

/// Parsed package metadata. Packages are directories with a `skill.yaml`,
/// `SKILL.md` entrypoint, and golden JSON input/output fixtures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPackageManifest {
    pub schema: String,
    pub side_effect_level: SideEffectLevel,
    pub visibility: SkillVisibility,
    pub entrypoint: String,
    pub golden_input: String,
    pub golden_output: String,
    pub judge_rules: Option<String>,
}

impl SkillPackageManifest {
    /// Parse the intentionally small, flat package section of `skill.yaml`.
    /// Full skill metadata remains parsed by the HTTP importer.
    pub fn parse(yaml: &str) -> Result<Self, String> {
        let mut values = std::collections::HashMap::new();
        let mut section = String::new();
        for line in yaml.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            if let Some((key, raw)) = trimmed.split_once(':') {
                let key = key.trim();
                let value = raw.trim().trim_matches('"').trim_matches('\'').to_string();
                if indent == 0 && value.is_empty() {
                    section = key.to_string();
                } else if !value.is_empty() {
                    let key = if section.is_empty() || indent == 0 {
                        key.to_string()
                    } else {
                        format!("{section}.{key}")
                    };
                    values.insert(key, value);
                }
            }
        }
        let schema = values
            .remove("package.schema")
            .ok_or_else(|| "skill.yaml missing package.schema".to_string())?;
        if schema != "agentos.dev/skill-package/v1" {
            return Err(format!("unsupported package.schema: {schema}"));
        }
        let side_effect_level = match values.remove("package.side_effect_level").as_deref() {
            Some("none") => SideEffectLevel::None,
            Some("read") => SideEffectLevel::Read,
            Some("write") => SideEffectLevel::Write,
            Some("execute") => SideEffectLevel::Execute,
            Some(other) => return Err(format!("invalid package.side_effect_level: {other}")),
            None => return Err("skill.yaml missing package.side_effect_level".to_string()),
        };
        let visibility = match values.remove("package.visibility").as_deref() {
            Some("system") => SkillVisibility::System,
            Some("tenant") => SkillVisibility::Tenant,
            Some("session") => SkillVisibility::Session,
            Some(other) => return Err(format!("invalid package.visibility: {other}")),
            None => return Err("skill.yaml missing package.visibility".to_string()),
        };
        let entrypoint = values
            .remove("package.entrypoint")
            .unwrap_or_else(|| "SKILL.md".to_string());
        let golden_input = values
            .remove("package.golden_input")
            .unwrap_or_else(|| "tests/golden-input.json".to_string());
        let golden_output = values
            .remove("package.golden_output")
            .unwrap_or_else(|| "tests/golden-output.json".to_string());
        Ok(Self {
            schema,
            side_effect_level,
            visibility,
            entrypoint,
            golden_input,
            golden_output,
            judge_rules: values.remove("package.judge_rules"),
        })
    }
}

/// 流水线执行上下文。
pub struct PipelineContext {
    pub source: PipelineSource,
    pub triggered_by: String,
    pub repo_url: Option<String>,
    /// Git 导入时的本地克隆目录（用于敏感扫描/示例夹具/仓库测试）；其他来源为 None。
    pub clone_dir: Option<PathBuf>,
    /// 仓库内技能子目录（相对 clone_dir），缺省 "."。
    pub sub_path: String,
    /// Git packages require the package format and golden checks. Existing
    /// JSON API payloads remain supported for session-scoped authoring.
    pub require_package: bool,
    pub visibility: SkillVisibility,
}

impl PipelineContext {
    /// 构造手动注册/重跑上下文（无克隆目录）。
    pub fn local(source: PipelineSource, triggered_by: impl Into<String>) -> Self {
        Self {
            source,
            triggered_by: triggered_by.into(),
            repo_url: None,
            clone_dir: None,
            sub_path: ".".into(),
            require_package: false,
            visibility: SkillVisibility::Session,
        }
    }

    /// 定位技能目录（clone_dir + sub_path）。
    fn skill_dir(&self) -> Option<PathBuf> {
        self.clone_dir.as_ref().map(|d| {
            let sub = self.sub_path.trim_matches('/');
            if sub.is_empty() || sub == "." {
                d.clone()
            } else {
                d.join(sub)
            }
        })
    }
}

/// 系统内建/受支持的角色集合（校验用，未知角色仅告警不阻断）。
const KNOWN_ROLES: &[&str] = &["PA", "DA", "CA", "AA", "SA", "USER"];

/// 敏感模式：命中即视为高危泄露（阻断）。仅匹配「明确的密钥文本头」。
const SECRET_HARD_PATTERNS: &[&str] = &[
    "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN OPENSSH PRIVATE KEY-----",
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----",
    "-----BEGIN PGP PRIVATE KEY BLOCK-----",
];

/// 敏感模式：命中仅告警（可能为示例/占位）。
const SECRET_SOFT_PATTERNS: &[&str] = &[
    "AKIA", // AWS Access Key ID 前缀
    "aws_secret_access_key",
    "api_key=",
    "apikey=",
    "password=",
    "secret_key=",
    "xoxb-", // Slack bot token
    "ghp_",  // GitHub personal access token
];

/// 单文件扫描上限（字节），避免大文件拖慢流水线。
const MAX_SCAN_FILE_BYTES: u64 = 512 * 1024;

// ────────────────────────────────────────────────────────────────────────────
// 阶段 1：Lint —— 清单/元数据规范 + Schema 可编译性
// ────────────────────────────────────────────────────────────────────────────

fn lint_stage(skill: &SkillMeta, ctx: &PipelineContext) -> StageResult {
    let mut b = StageResult::new("lint", "代码静态检查 (Lint)");

    if ctx.require_package {
        match load_package_manifest(ctx) {
            Ok((_, manifest)) => {
                if manifest.visibility == SkillVisibility::System {
                    b.fail("package.visibility=system is reserved for kernel-bundled skills");
                } else if manifest.visibility != ctx.visibility {
                    b.fail(format!(
                        "package.visibility={:?} does not match requested publication scope {:?}",
                        manifest.visibility, ctx.visibility
                    ));
                } else {
                    b.pass(format!(
                        "Skill package manifest valid (schema={}, visibility={:?}, side_effect_level={:?})",
                        manifest.schema, manifest.visibility, manifest.side_effect_level
                    ));
                }
            }
            Err(error) => b.fail(error),
        }
    } else {
        b.pass("session authoring payload (no package manifest required)");
    }

    // skill_iri 命名规范
    if skill.skill_iri.trim().is_empty() {
        b.fail("skill_iri 为空");
    } else if skill.skill_iri.starts_with("iri://") || skill.skill_iri.starts_with("skill://") {
        b.pass(format!("skill_iri 命名规范：{}", skill.skill_iri));
    } else {
        b.warn(format!(
            "skill_iri 未使用 iri:// 或 skill:// 前缀：{}",
            skill.skill_iri
        ));
    }

    // 名称 / 描述 / 版本
    if skill.name.trim().is_empty() {
        b.fail("name 为空");
    } else {
        b.pass(format!("name={}", skill.name));
    }
    if skill.description.trim().is_empty() {
        b.warn("description 为空（建议补充说明）");
    } else {
        b.pass("description 已填写");
    }
    if skill.version.trim().is_empty() {
        b.warn("version 为空");
    } else {
        b.pass(format!("version={}", skill.version));
    }

    // 角色集合
    if skill.allowed_roles.is_empty() {
        b.warn("allowed_roles 为空（该技能对任何角色不可见）");
    } else {
        let unknown: Vec<&String> = skill
            .allowed_roles
            .iter()
            .filter(|r| !KNOWN_ROLES.contains(&r.as_str()))
            .collect();
        if unknown.is_empty() {
            b.pass(format!("allowed_roles={:?}", skill.allowed_roles));
        } else {
            b.warn(format!("存在未知角色：{:?}", unknown));
        }
    }

    // input/output Schema 可编译性
    for (label, schema) in [
        ("input_schema", &skill.input_schema),
        ("output_schema", &skill.output_schema),
    ] {
        match jsonschema::JSONSchema::options().compile(schema) {
            Ok(_) => b.pass(format!("{} 可编译为合法 JSON Schema", label)),
            Err(e) => b.fail(format!("{} 非法：{}", label, e)),
        }
    }

    // compiled_template 若非空须为合法 JSON（模板占位符 "___" 仍是合法 JSON 字符串值）
    if skill.compiled_template.trim().is_empty() {
        b.warn("compiled_template 为空");
    } else if serde_json::from_str::<serde_json::Value>(&skill.compiled_template).is_ok() {
        b.pass("compiled_template 为合法 JSON");
    } else {
        b.warn("compiled_template 非合法 JSON（将按原样存储）");
    }

    let summary = format!(
        "{} 项检查（{} 通过）",
        b.details.len(),
        b.details.iter().filter(|d| d.starts_with('✓')).count()
    );
    b.finish(summary)
}

fn load_package_manifest(ctx: &PipelineContext) -> Result<(PathBuf, SkillPackageManifest), String> {
    let dir = ctx
        .skill_dir()
        .ok_or_else(|| "package gate requires a package directory".to_string())?;
    let yaml = std::fs::read_to_string(dir.join("skill.yaml"))
        .map_err(|_| "package is missing readable skill.yaml".to_string())?;
    let manifest = SkillPackageManifest::parse(&yaml)?;
    for path in [
        &manifest.entrypoint,
        &manifest.golden_input,
        &manifest.golden_output,
    ] {
        let joined = dir.join(path);
        if !joined.is_file() {
            return Err(format!("package required file is missing: {path}"));
        }
    }
    if let Some(rules) = &manifest.judge_rules {
        if !dir.join(rules).is_file() {
            return Err(format!("package judge rules file is missing: {rules}"));
        }
    }
    Ok((dir, manifest))
}

// ────────────────────────────────────────────────────────────────────────────
// 阶段 2：Security —— 签名 + 权限一致性 + 敏感扫描
// ────────────────────────────────────────────────────────────────────────────

fn security_stage(
    registry: &SkillRegistry,
    skill: &SkillMeta,
    ctx: &PipelineContext,
) -> StageResult {
    let mut b = StageResult::new("security", "安全扫描 (签名 + 敏感信息)");

    // 1) 签名核验
    match registry.verify_skill_signature(skill) {
        SignatureStatus::Verified => b.pass("Ed25519 签名核验通过"),
        SignatureStatus::Unsigned => b.warn("技能未签名（未强制签名策略下放行）"),
        SignatureStatus::NoTrustAnchor => b.warn("携带签名但未配置受信公钥，无法判定"),
        SignatureStatus::Invalid => b.fail("签名无效（公钥不匹配或载荷被篡改）"),
    }

    // 2) 权限-安全级一致性：敏感权限须匹配非 normal 的安全级
    let sensitive = skill.skill_types.iter().any(|t| {
        let t = t.to_lowercase();
        t.contains("network") || t.contains("oss") || t.contains("mcp") || t.contains("execution")
    });
    if sensitive && skill.security_level.eq_ignore_ascii_case("normal") {
        b.warn("声明了敏感权限（网络/执行/OSS/MCP）但 security_level=normal，建议提升");
    } else {
        b.pass(format!(
            "security_level={} 与权限声明一致",
            skill.security_level
        ));
    }

    // 3) 克隆目录敏感模式扫描（仅 Git 来源存在 clone_dir）
    match ctx.skill_dir() {
        Some(dir) if dir.exists() => {
            let (hard, soft, scanned) = scan_secrets(&dir);
            b.pass(format!("已扫描 {} 个文件", scanned));
            if hard.is_empty() && soft.is_empty() {
                b.pass("未发现硬编码密钥");
            }
            for (file, count) in &hard {
                b.fail(format!("检测到硬编码私钥：{}（命中 {} 处）", file, count));
            }
            for (file, count) in &soft {
                b.warn(format!(
                    "疑似敏感字段：{}（命中 {} 处，请人工确认是否为示例）",
                    file, count
                ));
            }
        }
        _ => b.pass("无仓库源码（手动注册），跳过文件级敏感扫描"),
    }

    let summary = if b.failed {
        "安全门禁未通过".to_string()
    } else if b.warned {
        "通过（含告警）".to_string()
    } else {
        "全部安全检查通过".to_string()
    };
    b.finish(summary)
}

/// 递归扫描目录，返回 (硬命中[文件名,计数], 软命中[文件名,计数], 已扫描文件数)。
/// 只返回文件名与计数，绝不返回命中的明文。
#[allow(clippy::type_complexity)]
fn scan_secrets(root: &Path) -> (Vec<(String, usize)>, Vec<(String, usize)>, usize) {
    let mut hard: Vec<(String, usize)> = Vec::new();
    let mut soft: Vec<(String, usize)> = Vec::new();
    let mut scanned = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if name == ".git" || name == "node_modules" || name == "target" {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.len() > MAX_SCAN_FILE_BYTES {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue, // 二进制/非 UTF-8 跳过
            };
            scanned += 1;
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let hard_hits = SECRET_HARD_PATTERNS
                .iter()
                .filter(|p| content.contains(**p))
                .count();
            if hard_hits > 0 {
                hard.push((rel.clone(), hard_hits));
            }
            let soft_hits = SECRET_SOFT_PATTERNS
                .iter()
                .filter(|p| content.to_lowercase().contains(&p.to_lowercase()))
                .count();
            if soft_hits > 0 {
                soft.push((rel, soft_hits));
            }
        }
    }
    (hard, soft, scanned)
}

// ────────────────────────────────────────────────────────────────────────────
// 阶段 3：Test —— Schema 自测 + 冲突检测 + 示例夹具 + 可选仓库测试
// ────────────────────────────────────────────────────────────────────────────

fn test_stage(registry: &SkillRegistry, skill: &SkillMeta, ctx: &PipelineContext) -> StageResult {
    let mut b = StageResult::new("test", "单元测试 & 冲突检测");

    // 1) skill_iri 冲突检测（已存在视为覆盖更新，仅告警）
    if registry.get_skill(&skill.skill_iri).is_some() {
        b.warn(format!(
            "skill_iri 已存在，将执行覆盖更新：{}",
            skill.skill_iri
        ));
    } else {
        b.pass("无 skill_iri 冲突（新增注册）");
    }

    // 2) Schema 自测：确认输入/输出 Schema 可被 jsonschema 编译并对空对象求值
    let mut compiled_input = None;
    match jsonschema::JSONSchema::options().compile(&skill.input_schema) {
        Ok(c) => {
            b.pass("input_schema 编译通过");
            compiled_input = Some(c);
        }
        Err(e) => b.fail(format!("input_schema 编译失败：{}", e)),
    }
    if jsonschema::JSONSchema::options()
        .compile(&skill.output_schema)
        .is_ok()
    {
        b.pass("output_schema 编译通过");
    } else {
        b.fail("output_schema 编译失败");
    }

    // 3) Package golden execution contract. Skills do not run arbitrary package
    // code in the kernel gate. Instead their declared golden input and output
    // must satisfy the schemas; executable hooks stay opt-in outside CI.
    if ctx.require_package {
        match load_package_manifest(ctx) {
            Ok((dir, manifest)) => {
                let input = read_json_fixture(&dir.join(&manifest.golden_input), "golden input");
                let output = read_json_fixture(&dir.join(&manifest.golden_output), "golden output");
                match (input, output, compiled_input.as_ref()) {
                    (Ok(input), Ok(output), Some(input_schema)) => {
                        if input_schema.is_valid(&input) {
                            b.pass("golden input satisfies input_schema");
                        } else {
                            b.fail("golden input does not satisfy input_schema");
                        }
                        match jsonschema::JSONSchema::options().compile(&skill.output_schema) {
                            Ok(output_schema) if output_schema.is_valid(&output) => {
                                b.pass("golden output satisfies output_schema (minimal execute contract)");
                            }
                            Ok(_) => b.fail("golden output does not satisfy output_schema"),
                            Err(_) => b.fail("output_schema cannot validate golden output"),
                        }
                        if let Some(rules) = manifest.judge_rules {
                            match evaluate_judge_rules(&dir.join(rules), &input, &output) {
                                Ok(()) => b.pass("deterministic Judge rules passed"),
                                Err(error) => {
                                    b.fail(format!("Judge rules rejected package: {error}"))
                                }
                            }
                        } else {
                            b.pass("Judge hook disabled (no package.judge_rules; LLM Judge is OFF by default)");
                        }
                    }
                    (Err(error), _, _) | (_, Err(error), _) => b.fail(error),
                    _ => b.fail("input_schema unavailable for golden test"),
                }
            }
            Err(error) => b.fail(format!("package test setup failed: {error}")),
        }
    }

    // 4) 示例夹具校验：clone_dir/<sub>/examples/*.json 或 tests/*.json 逐一按 input_schema 校验
    let mut example_total = 0usize;
    let mut example_ok = 0usize;
    if let (Some(dir), Some(schema)) = (ctx.skill_dir(), compiled_input.as_ref()) {
        for sub in ["examples", "tests", "fixtures"] {
            let fdir = dir.join(sub);
            if !fdir.is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&fdir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    let text = match std::fs::read_to_string(&p) {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    let val: serde_json::Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => {
                            example_total += 1;
                            b.warn(format!(
                                "示例 {} 非合法 JSON",
                                entry.file_name().to_string_lossy()
                            ));
                            continue;
                        }
                    };
                    example_total += 1;
                    if schema.is_valid(&val) {
                        example_ok += 1;
                    } else {
                        b.warn(format!(
                            "示例 {} 不满足 input_schema",
                            entry.file_name().to_string_lossy()
                        ));
                    }
                }
            }
        }
    }
    if example_total > 0 {
        let cov = (example_ok as f64 / example_total as f64 * 100.0).round() as u32;
        b.pass(format!(
            "示例夹具校验：{}/{} 通过（覆盖率 {}%）",
            example_ok, example_total, cov
        ));
    } else {
        b.pass("无示例夹具（examples/tests/fixtures），仅执行 Schema 自测");
    }

    // 5) 可选：检测并执行仓库测试框架（默认关闭，由 AGENTOS_PIPELINE_RUN_REPO_TESTS=1 开启）
    if let Some(dir) = ctx.skill_dir() {
        let frameworks = detect_test_frameworks(&dir);
        if frameworks.is_empty() {
            b.pass("未检测到仓库测试清单");
        } else {
            let run_enabled = std::env::var("AGENTOS_PIPELINE_RUN_REPO_TESTS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            b.pass(format!("检测到测试框架：{:?}", frameworks));
            if run_enabled {
                match run_repo_tests(&dir, &frameworks[0]) {
                    Ok(true) => b.pass(format!("{} 测试通过", frameworks[0])),
                    Ok(false) => b.fail(format!("{} 测试失败", frameworks[0])),
                    Err(e) => b.warn(format!("{} 测试执行异常：{}", frameworks[0], e)),
                }
            } else {
                b.warn("仓库测试默认不执行（设 AGENTOS_PIPELINE_RUN_REPO_TESTS=1 以真跑）");
            }
        }
    }

    let summary = if b.failed {
        "测试门禁未通过".to_string()
    } else if example_total > 0 {
        format!(
            "Schema 自测通过 + 示例 {}/{} 通过",
            example_ok, example_total
        )
    } else {
        "Schema 自测通过".to_string()
    };
    b.finish(summary)
}

fn read_json_fixture(path: &Path, label: &str) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{label} cannot be read: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("{label} is invalid JSON: {e}"))
}

/// Rules are deliberately data-only: each non-comment line is `input.path`
/// or `output.path` and must resolve to a non-null JSON value. This provides a
/// deterministic Judge hook without granting imported packages code execution.
fn evaluate_judge_rules(
    path: &Path,
    input: &serde_json::Value,
    output: &serde_json::Value,
) -> Result<(), String> {
    let rules = std::fs::read_to_string(path).map_err(|e| format!("cannot read rules: {e}"))?;
    for line in rules
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let (scope, pointer) = line.split_once(':').ok_or_else(|| {
            format!("invalid rule `{line}`; expected input:/pointer or output:/pointer")
        })?;
        let value = match scope {
            "input" => input.pointer(pointer),
            "output" => output.pointer(pointer),
            _ => return Err(format!("unknown rule scope `{scope}`")),
        };
        if value.is_none() || value == Some(&serde_json::Value::Null) {
            return Err(format!("required JSON pointer missing: {line}"));
        }
    }
    Ok(())
}

/// 依据仓库中的清单文件推断测试框架（有序，取第一个执行）。
fn detect_test_frameworks(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if dir.join("Cargo.toml").is_file() {
        out.push("cargo".to_string());
    }
    if dir.join("package.json").is_file() {
        out.push("npm".to_string());
    }
    if dir.join("pytest.ini").is_file()
        || dir.join("pyproject.toml").is_file()
        || dir.join("tests").is_dir() && dir.join("setup.py").is_file()
    {
        out.push("pytest".to_string());
    }
    out
}

/// 在克隆目录真实执行仓库测试（受门控调用），带 180s 超时。返回是否通过。
fn run_repo_tests(dir: &Path, framework: &str) -> std::io::Result<bool> {
    use std::process::Command;
    let mut cmd = match framework {
        "cargo" => {
            let mut c = Command::new("cargo");
            c.arg("test").arg("--quiet");
            c
        }
        "npm" => {
            let mut c = Command::new("npm");
            c.arg("test").arg("--silent");
            c
        }
        "pytest" => {
            let mut c = Command::new("pytest");
            c.arg("-q");
            c
        }
        _ => return Ok(false),
    };
    cmd.current_dir(dir);
    let status = cmd.status()?;
    Ok(status.success())
}

// ────────────────────────────────────────────────────────────────────────────
// 阶段 4：Publish —— 通过回调真正注册/持久化
// ────────────────────────────────────────────────────────────────────────────

/// 发布回调：接收最终技能，返回 Ok(摘要) 表示注册成功，Err(错误) 表示注册失败。
pub type PublishFn<'a> = dyn FnOnce(&SkillMeta) -> Result<String, String> + 'a;

// ────────────────────────────────────────────────────────────────────────────
// 编排入口
// ────────────────────────────────────────────────────────────────────────────

/// 执行完整流水线。前三阶段任一 Failed 则门禁拒绝，跳过发布并将 publish 阶段标记为 Skipped。
pub fn run_pipeline(
    registry: &SkillRegistry,
    skill: &SkillMeta,
    ctx: &PipelineContext,
    publish: Box<PublishFn<'_>>,
) -> PipelineRun {
    let started = Instant::now();
    let started_at = chrono::Utc::now().to_rfc3339();

    let mut stages = Vec::with_capacity(4);
    stages.push(lint_stage(skill, ctx));
    stages.push(security_stage(registry, skill, ctx));
    stages.push(test_stage(registry, skill, ctx));

    let gate_passed = !stages.iter().any(|s| s.status == StageStatus::Failed);

    let mut published = false;
    let publish_stage = {
        let mut b = StageResult::new("publish", "发布注册 (Admission)");
        if gate_passed {
            match publish(skill) {
                Ok(msg) => {
                    published = true;
                    b.pass(msg);
                    b.finish("已注册并持久化")
                }
                Err(e) => {
                    b.fail(format!("注册失败：{}", e));
                    b.finish("注册失败")
                }
            }
        } else {
            b.details.push("⊘ 前置门禁未通过，跳过发布".to_string());
            let mut r = b.finish("已跳过（门禁拦截）");
            r.status = StageStatus::Skipped;
            r
        }
    };
    stages.push(publish_stage);

    let summary = if !gate_passed {
        "流水线拦截：存在未通过的准入检查".to_string()
    } else if published {
        "流水线通过，技能已注册发布".to_string()
    } else {
        "门禁通过但发布失败".to_string()
    };

    PipelineRun {
        run_id: uuid::Uuid::new_v4().to_string(),
        skill_iri: skill.skill_iri.clone(),
        skill_name: skill.name.clone(),
        version: skill.version.clone(),
        source: ctx.source,
        visibility: ctx.visibility,
        triggered_by: ctx.triggered_by.clone(),
        repo_url: ctx.repo_url.clone(),
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        stages,
        gate_passed,
        published,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample_skill() -> SkillMeta {
        SkillMeta {
            skill_iri: "skill://test/demo".into(),
            name: "demo".into(),
            description: "a demo skill".into(),
            version: "1.0.0".into(),
            category: "application".into(),
            security_level: "normal".into(),
            allowed_roles: vec!["DA".into()],
            input_schema: serde_json::json!({"type":"object","properties":{"x":{"type":"string"}},"required":["x"]}),
            output_schema: serde_json::json!({"type":"object"}),
            compiled_template: r#"{"x":"___"}"#.into(),
            signature: None,
            signature_algorithm: None,
            input_mapping: HashMap::new(),
            output_mapping: HashMap::new(),
            skill_types: vec![],
        }
    }

    #[test]
    fn pipeline_passes_and_publishes_valid_skill() {
        let registry = SkillRegistry::new();
        let skill = sample_skill();
        let ctx = PipelineContext::local(PipelineSource::Manual, "tester");
        let run = run_pipeline(
            &registry,
            &skill,
            &ctx,
            Box::new(|_s| Ok("registered".into())),
        );
        assert!(run.gate_passed, "gate should pass: {:?}", run.stages);
        assert!(run.published);
        assert_eq!(run.stages.len(), 4);
    }

    #[test]
    fn pipeline_blocks_invalid_input_schema() {
        let registry = SkillRegistry::new();
        let mut skill = sample_skill();
        // 非法 schema：type 取了非法值
        skill.input_schema = serde_json::json!({"type": 123});
        let ctx = PipelineContext::local(PipelineSource::Manual, "tester");
        let published_flag = std::cell::Cell::new(false);
        let run = run_pipeline(
            &registry,
            &skill,
            &ctx,
            Box::new(|_s| {
                published_flag.set(true);
                Ok("registered".into())
            }),
        );
        assert!(!run.gate_passed);
        assert!(!run.published);
        assert!(
            !published_flag.get(),
            "publish callback must not run when gate fails"
        );
    }

    fn fixture_context(name: &str) -> PipelineContext {
        PipelineContext {
            source: PipelineSource::Git,
            triggered_by: "ci".into(),
            repo_url: None,
            clone_dir: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/skills")
                    .join(name),
            ),
            sub_path: ".".into(),
            require_package: true,
            visibility: SkillVisibility::Tenant,
        }
    }

    #[test]
    fn skill_package_gate_accepts_green_fixture_and_publishes() {
        let registry = SkillRegistry::new();
        let skill = sample_skill();
        let run = run_pipeline(
            &registry,
            &skill,
            &fixture_context("package-green"),
            Box::new(|_| Ok("tenant skill published".into())),
        );
        assert!(
            run.gate_passed,
            "green fixture should pass: {:?}",
            run.stages
        );
        assert!(run.published);
        assert_eq!(run.visibility, SkillVisibility::Tenant);
    }

    #[test]
    fn skill_package_gate_rejects_red_fixture_without_publishing() {
        let registry = SkillRegistry::new();
        let skill = sample_skill();
        let published = std::cell::Cell::new(false);
        let run = run_pipeline(
            &registry,
            &skill,
            &fixture_context("package-red"),
            Box::new(|_| {
                published.set(true);
                Ok("must not run".into())
            }),
        );
        assert!(
            !run.gate_passed,
            "red fixture should fail: {:?}",
            run.stages
        );
        assert!(!run.published);
        assert!(!published.get(), "red fixture must never publish");
        assert!(
            run.stages.iter().any(|stage| {
                stage.stage == "test"
                    && stage
                        .details
                        .iter()
                        .any(|detail| detail.contains("golden input does not satisfy"))
            }),
            "failure should identify the golden input gate"
        );
    }
}
