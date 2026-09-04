//! 本体元定义存储（OntologyStore）——把硬编码只读本体升级为存储驱动、可在线 CRUD 的元模型。
//!
//! 采用 **Hybrid** 存储策略（决策见实施计划 §3）：
//!   1. 规范三元组投影：写入元命名图 `graph:ontology/meta`，供 SPARQL/SHACL/推理；
//!   2. `meta:json` 无损快照：在元素 IRI 上写入 canonical JSON 字符串，作为**读路径来源**
//!      （保序、含可选字段、反构造零歧义）。
//!
//! 读优先解析 `meta:json` 重建 `OntologyDefinition`；写按单条记录"先删后插"保证幂等原子。
//! Phase 0 仅实现：地基 + 幂等 seed + `load_definition`（读迁移）。CRUD 于后续阶段叠加。

use oxigraph::io::RdfFormat;
use oxigraph::model::{GraphNameRef, NamedNodeRef};
use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;
use std::sync::Arc;

use super::ontology_layer::{
    ev_repair_ontology, ActionGuardrailConfig, ActionType, FunctionDef, LinkType, ObjectType,
    OntologyDefinition,
};

/// 本体元定义命名图（与实例图 `graph:pack/ev-repair` 隔离）。
pub const META_GRAPH: &str = "graph:ontology/meta";
/// meta 命名空间前缀。
pub const META_NS: &str = "https://agentos.ontology/meta/";
/// RDF/RDFS 常量。
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";

/// 元素类别（决定 IRI 铸造与 meta:kind 标注）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaKind {
    ObjectType,
    LinkType,
    ActionType,
    FunctionDef,
}

impl MetaKind {
    /// meta 元类 IRI（如 `meta:ObjectType`）。
    fn class_iri(self) -> String {
        let name = match self {
            MetaKind::ObjectType => "ObjectType",
            MetaKind::LinkType => "LinkType",
            MetaKind::ActionType => "ActionType",
            MetaKind::FunctionDef => "FunctionDef",
        };
        format!("{}{}", META_NS, name)
    }
    /// 域内元素 IRI 前缀片段（与 `ontology_layer::ev` 语义一致）。
    fn iri_segment(self) -> &'static str {
        match self {
            MetaKind::ActionType => "action",
            MetaKind::FunctionDef => "function",
            _ => "",
        }
    }
}

/// 本体元定义存储（复用统一 Oxigraph Store 的 `Arc` 句柄）。
pub struct OntologyStore {
    store: Arc<Store>,
}

impl OntologyStore {
    /// 基于共享 Oxigraph Store 构造（与 `KnowledgeGraphStore::with_shared_store` 对齐）。
    pub fn with_shared_store(store: Arc<Store>) -> Result<Self, String> {
        Ok(Self { store })
    }

    /// 内存态构造（测试用）。
    pub fn new() -> Result<Self, String> {
        let store = Store::new().map_err(|e| format!("failed to create Oxigraph Store: {}", e))?;
        Ok(Self {
            store: Arc::new(store),
        })
    }

    /// 元素 IRI：ObjectType/LinkType 复用 `ev(id)`；Action/Function 带 segment 前缀。
    fn meta_iri(kind: MetaKind, id: &str) -> String {
        match kind.iri_segment() {
            "" => super::ontology_layer::ev(id),
            seg => super::ontology_layer::ev(&format!("{}/{}", seg, id)),
        }
    }

    /// SPARQL 字符串字面量转义（与 store.rs 私有实现一致，避免跨模块可见性改动）。
    fn escape_literal(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    }

    /// 元命名图是否已有指定 domain 的本体数据（用于幂等判定）。
    #[allow(deprecated)]
    fn is_seeded(&self, domain: &str) -> bool {
        let q = format!(
            "SELECT ?s WHERE {{ GRAPH <{g}> {{ ?s <{ns}domain> \"{d}\" }} }} LIMIT 1",
            g = META_GRAPH,
            ns = META_NS,
            d = Self::escape_literal(domain),
        );
        match self.store.query(&q) {
            Ok(QueryResults::Solutions(mut it)) => it.next().is_some(),
            _ => false,
        }
    }

    /// 幂等 seed：元命名图无该 domain 数据时，写入硬编码本体（当前仅 `ev-repair`）。
    /// 返回是否实际写入（true=首次 seed）。
    pub fn ensure_seeded(&self, domain: &str) -> Result<bool, String> {
        if self.is_seeded(domain) {
            return Ok(false);
        }
        let def = match domain {
            "ev-repair" => ev_repair_ontology(),
            other => return Err(format!("未知本体域: {}", other)),
        };
        self.write_definition(&def)?;
        let _ = self.store.flush();
        Ok(true)
    }

    /// 把一份完整本体定义写入元命名图（domain 标记 + 四类元素）。
    /// 用于 seed；后续 CRUD 复用 `encode_element` 做单记录 upsert。
    fn write_definition(&self, def: &OntologyDefinition) -> Result<(), String> {
        let mut triples: Vec<String> = Vec::new();
        // domain 标记（挂在一个稳定的 domain 节点上，供 is_seeded 判定与列举）。
        let domain_iri = format!("{}domain/{}", META_NS, iri_frag(&def.domain));
        triples.push(fmt_lit(
            &domain_iri,
            &format!("{}domain", META_NS),
            &def.domain,
        ));
        triples.push(fmt_iri(
            &domain_iri,
            RDF_TYPE,
            &format!("{}Domain", META_NS),
        ));
        triples.push(fmt_lit(
            &domain_iri,
            &format!("{}guardrails", META_NS),
            &serde_json::to_string(&def.guardrails).map_err(|e| e.to_string())?,
        ));

        for (i, o) in def.object_types.iter().enumerate() {
            triples.extend(self.encode_element(
                MetaKind::ObjectType,
                &o.id,
                &o.label,
                i,
                serde_json::to_value(o).map_err(|e| e.to_string())?,
                Some(&domain_iri),
            )?);
        }
        for (i, l) in def.link_types.iter().enumerate() {
            triples.extend(self.encode_element(
                MetaKind::LinkType,
                &l.id,
                &l.label,
                i,
                serde_json::to_value(l).map_err(|e| e.to_string())?,
                Some(&domain_iri),
            )?);
        }
        for (i, a) in def.action_types.iter().enumerate() {
            triples.extend(self.encode_element(
                MetaKind::ActionType,
                &a.id,
                &a.label,
                i,
                serde_json::to_value(a).map_err(|e| e.to_string())?,
                Some(&domain_iri),
            )?);
        }
        for (i, f) in def.functions.iter().enumerate() {
            triples.extend(self.encode_element(
                MetaKind::FunctionDef,
                &f.id,
                &f.label,
                i,
                serde_json::to_value(f).map_err(|e| e.to_string())?,
                Some(&domain_iri),
            )?);
        }

        let sparql = format!(
            "INSERT DATA {{ GRAPH <{g}> {{\n  {}\n}} }}",
            triples.join("\n  "),
            g = META_GRAPH,
        );
        self.store
            .update(&sparql)
            .map_err(|e| format!("本体 seed 写入失败: {}", e))
    }

    /// 编码单个元素为规范三元组（Hybrid）：
    ///   `<iri> a meta:<Kind>` · `rdfs:label` · `meta:id` · `meta:order` · `meta:json <snapshot>` ·
    ///   （可选）`<domainIri> meta:hasElement <iri>` 反向挂接。
    /// `meta:order`（xsd:integer）保序，读回按其排序，确保与 seed 输入顺序一致。
    /// canonical JSON 快照是读路径的权威来源，规范三元组供 SPARQL/SHACL。
    fn encode_element(
        &self,
        kind: MetaKind,
        id: &str,
        label: &str,
        order: usize,
        value: serde_json::Value,
        domain_iri: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let iri = Self::meta_iri(kind, id);
        let json = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        let mut t = vec![
            fmt_iri(&iri, RDF_TYPE, &kind.class_iri()),
            fmt_lit(&iri, RDFS_LABEL, label),
            fmt_lit(&iri, &format!("{}id", META_NS), id),
            fmt_int(&iri, &format!("{}order", META_NS), order as i64),
            fmt_lit(&iri, &format!("{}json", META_NS), &json),
        ];
        if let Some(d) = domain_iri {
            t.push(fmt_iri(d, &format!("{}hasElement", META_NS), &iri));
        }
        Ok(t)
    }

    /// 从元命名图读回指定 domain 的完整本体定义（读路径：解析 `meta:json` 快照）。
    /// 按元类分组查询，保证与 seed 输入逐字段一致（Hybrid 读优先 JSON）。
    pub fn load_definition(&self, domain: &str) -> Result<OntologyDefinition, String> {
        let guardrails = self.load_domain_guardrails(domain)?;
        let object_types = self
            .load_snapshots(MetaKind::ObjectType)?
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let link_types = self
            .load_snapshots(MetaKind::LinkType)?
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let action_types = self
            .load_snapshots(MetaKind::ActionType)?
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let functions = self
            .load_snapshots(MetaKind::FunctionDef)?
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(OntologyDefinition {
            domain: domain.to_string(),
            guardrails,
            object_types,
            link_types,
            action_types,
            functions,
        })
    }

    /// 读取域级护栏 JSON；旧数据没有该谓词时返回空配置，以保持现有安全默认值。
    #[allow(deprecated)]
    pub fn load_domain_guardrails(&self, domain: &str) -> Result<ActionGuardrailConfig, String> {
        let domain_iri = Self::domain_iri(domain);
        let q = format!(
            "SELECT ?json WHERE {{ GRAPH <{g}> {{ <{domain_iri}> <{ns}guardrails> ?json }} }} LIMIT 1",
            g = META_GRAPH,
            ns = META_NS,
        );
        match self.store.query(&q).map_err(|e| e.to_string())? {
            QueryResults::Solutions(mut solutions) => match solutions.next() {
                Some(Ok(solution)) => {
                    let raw = solution
                        .get("json")
                        .map(|term| strip_literal(&term.to_string()))
                        .ok_or_else(|| "域护栏配置读取失败".to_string())?;
                    serde_json::from_str(&raw).map_err(|e| format!("域护栏配置无效: {e}"))
                }
                Some(Err(e)) => Err(e.to_string()),
                None => Ok(ActionGuardrailConfig::default()),
            },
            _ => Ok(ActionGuardrailConfig::default()),
        }
    }

    /// 更新一个域的默认护栏配置。调用者须先通过 HTTP 层的 verified claims 校验。
    pub fn upsert_domain_guardrails(
        &self,
        domain: &str,
        guardrails: &ActionGuardrailConfig,
    ) -> Result<(), String> {
        let domain_iri = Self::domain_iri(domain);
        let predicate = format!("{}guardrails", META_NS);
        let value = serde_json::to_string(guardrails).map_err(|e| e.to_string())?;
        let _ = self.backup_meta_graph();
        self.store
            .update(&format!(
                "DELETE WHERE {{ GRAPH <{g}> {{ <{domain_iri}> <{predicate}> ?o }} }};\
                 INSERT DATA {{ GRAPH <{g}> {{ <{domain_iri}> <{predicate}> \"{}\" }} }}",
                Self::escape_literal(&value),
                g = META_GRAPH,
            ))
            .map_err(|e| format!("写入域护栏配置失败: {e}"))?;
        let _ = self.store.flush();
        Ok(())
    }

    /// 读取某一元类下全部元素的 `meta:json` 快照，按 `meta:order`（数值）稳定排序，
    /// 确保读回顺序与 seed 写入顺序一致。
    #[allow(deprecated)]
    fn load_snapshots(&self, kind: MetaKind) -> Result<Vec<serde_json::Value>, String> {
        let q = format!(
            "SELECT ?json ?ord WHERE {{ GRAPH <{g}> {{ \
               ?s a <{cls}> . \
               ?s <{ns}json> ?json . \
               ?s <{ns}order> ?ord . \
             }} }} ORDER BY ?ord",
            g = META_GRAPH,
            cls = kind.class_iri(),
            ns = META_NS,
        );
        let results = self
            .store
            .query(&q)
            .map_err(|e| format!("本体读取失败: {}", e))?;
        let mut out = Vec::new();
        if let QueryResults::Solutions(solutions) = results {
            for sol in solutions {
                let sol = sol.map_err(|e| e.to_string())?;
                if let Some(term) = sol.get("json") {
                    let raw = term.to_string();
                    let json_str = strip_literal(&raw);
                    let v: serde_json::Value =
                        serde_json::from_str(&json_str).map_err(|e| e.to_string())?;
                    out.push(v);
                }
            }
        }
        Ok(out)
    }

    // ── 阶段1：ObjectType + LinkType 在线 CRUD ──────────────────────────────
    //
    // 写策略：单记录「先删后插」保证幂等原子；写前对整张 meta 图做 N-Quads 备份。
    // 删除策略：先做引用完整性校验（被链接/动作/函数引用则拒绝），再删该记录五元组
    //           与 domain 反向挂接。所有写操作复用 domain 反向挂接维持列举一致。

    /// domain 节点 IRI（与 write_definition 一致）。
    fn domain_iri(domain: &str) -> String {
        format!("{}domain/{}", META_NS, iri_frag(domain))
    }

    /// 读取某一元类下全部元素 id（用于引用完整性/存在性判定）。
    #[allow(deprecated)]
    fn load_ids(&self, kind: MetaKind) -> Result<Vec<String>, String> {
        let q = format!(
            "SELECT ?id WHERE {{ GRAPH <{g}> {{ ?s a <{cls}> . ?s <{ns}id> ?id . }} }}",
            g = META_GRAPH,
            cls = kind.class_iri(),
            ns = META_NS,
        );
        let mut out = Vec::new();
        if let Ok(QueryResults::Solutions(sols)) = self.store.query(&q) {
            for sol in sols {
                let sol = sol.map_err(|e| e.to_string())?;
                if let Some(t) = sol.get("id") {
                    out.push(strip_literal(&t.to_string()));
                }
            }
        }
        Ok(out)
    }

    /// 元素在图中的当前 `meta:order`（无则返回下一个可用序号=同类计数）。
    #[allow(deprecated)]
    fn current_order(&self, kind: MetaKind, id: &str) -> Result<usize, String> {
        let iri = Self::meta_iri(kind, id);
        let q = format!(
            "SELECT ?ord WHERE {{ GRAPH <{g}> {{ <{iri}> <{ns}order> ?ord . }} }} LIMIT 1",
            g = META_GRAPH,
            iri = iri,
            ns = META_NS,
        );
        if let Ok(QueryResults::Solutions(mut sols)) = self.store.query(&q) {
            if let Some(Ok(sol)) = sols.next() {
                if let Some(t) = sol.get("ord") {
                    if let Ok(n) = strip_literal(&t.to_string()).parse::<usize>() {
                        return Ok(n);
                    }
                }
            }
        }
        Ok(self.load_ids(kind)?.len())
    }

    /// 备份整张 meta 命名图到 `<data>/ontology_backups/meta-<ts>.nq`。best-effort，返回路径。
    fn backup_meta_graph(&self) -> Result<std::path::PathBuf, String> {
        let dir = crate::api::http::data_dir().join("ontology_backups");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建备份目录失败: {}", e))?;
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S%3f");
        let path = dir.join(format!("meta-{}.nq", ts));
        let g = NamedNodeRef::new(META_GRAPH).map_err(|e| e.to_string())?;
        let buf = self
            .store
            .dump_graph_to_writer(GraphNameRef::from(g), RdfFormat::NQuads, Vec::new())
            .map_err(|e| format!("meta 图导出失败: {}", e))?;
        std::fs::write(&path, buf).map_err(|e| format!("写入备份失败: {}", e))?;
        Ok(path)
    }

    /// 删除单条元素记录的全部五元组 + domain 反向挂接（不做引用校验，供 upsert/delete 内部复用）。
    fn delete_element_quads(&self, kind: MetaKind, id: &str, domain: &str) -> Result<(), String> {
        let iri = Self::meta_iri(kind, id);
        let dom = Self::domain_iri(domain);
        let sparql = format!(
            "DELETE WHERE {{ GRAPH <{g}> {{ <{iri}> ?p ?o . }} }};\n\
             DELETE WHERE {{ GRAPH <{g}> {{ <{dom}> <{ns}hasElement> <{iri}> . }} }}",
            g = META_GRAPH,
            iri = iri,
            dom = dom,
            ns = META_NS,
        );
        self.store
            .update(&sparql)
            .map_err(|e| format!("删除元素失败: {}", e))
    }

    /// 写入单条元素记录（先删后插，幂等原子）。order 缺省沿用现有或追加到末尾。
    fn write_element(
        &self,
        kind: MetaKind,
        id: &str,
        label: &str,
        value: serde_json::Value,
        domain: &str,
    ) -> Result<(), String> {
        let order = self.current_order(kind, id)?;
        self.delete_element_quads(kind, id, domain)?;
        let dom = Self::domain_iri(domain);
        let triples = self.encode_element(kind, id, label, order, value, Some(&dom))?;
        let sparql = format!(
            "INSERT DATA {{ GRAPH <{g}> {{\n  {}\n}} }}",
            triples.join("\n  "),
            g = META_GRAPH,
        );
        self.store
            .update(&sparql)
            .map_err(|e| format!("写入元素失败: {}", e))?;
        let _ = self.store.flush();
        Ok(())
    }

    /// upsert ObjectType（新建或整体替换）。写前备份 meta 图。
    pub fn upsert_object_type(&self, domain: &str, obj: &ObjectType) -> Result<(), String> {
        let _ = self.backup_meta_graph();
        let value = serde_json::to_value(obj).map_err(|e| e.to_string())?;
        self.write_element(MetaKind::ObjectType, &obj.id, &obj.label, value, domain)
    }

    /// upsert LinkType（新建或整体替换）。校验 source/target 引用存在；写前备份 meta 图。
    pub fn upsert_link_type(&self, domain: &str, link: &LinkType) -> Result<(), String> {
        let obj_ids = self.load_ids(MetaKind::ObjectType)?;
        let mut missing = Vec::new();
        if !obj_ids.iter().any(|o| o == &link.source) {
            missing.push(format!("source={}", link.source));
        }
        if !obj_ids.iter().any(|o| o == &link.target) {
            missing.push(format!("target={}", link.target));
        }
        if !missing.is_empty() {
            return Err(format!("链接引用的对象类型不存在: {}", missing.join(", ")));
        }
        let _ = self.backup_meta_graph();
        let value = serde_json::to_value(link).map_err(|e| e.to_string())?;
        self.write_element(MetaKind::LinkType, &link.id, &link.label, value, domain)
    }

    /// 引用完整性：返回引用该 ObjectType 的下游元素描述列表（非空即不可删）。
    pub fn object_type_references(&self, id: &str) -> Result<Vec<String>, String> {
        let def = self.load_definition("")?;
        let mut refs = Vec::new();
        for l in &def.link_types {
            if l.source == id {
                refs.push(format!("链接 {} 的 source", l.label));
            }
            if l.target == id {
                refs.push(format!("链接 {} 的 target", l.label));
            }
        }
        for a in &def.action_types {
            if a.applies_to == id {
                refs.push(format!("动作 {} 的 applies_to", a.label));
            }
        }
        for f in &def.functions {
            if f.applies_to == id {
                refs.push(format!("函数 {} 的 applies_to", f.label));
            }
        }
        Ok(refs)
    }

    /// 删除 ObjectType。若被链接/动作/函数引用则返回冲突列表（调用方转 409）。写前备份。
    pub fn delete_object_type(&self, domain: &str, id: &str) -> Result<(), Vec<String>> {
        let refs = self.object_type_references(id).map_err(|e| vec![e])?;
        if !refs.is_empty() {
            return Err(refs);
        }
        let _ = self.backup_meta_graph();
        self.delete_element_quads(MetaKind::ObjectType, id, domain)
            .map_err(|e| vec![e])?;
        let _ = self.store.flush();
        Ok(())
    }

    /// 删除 LinkType（链接无下游引用，直接删）。写前备份。
    pub fn delete_link_type(&self, domain: &str, id: &str) -> Result<(), String> {
        let _ = self.backup_meta_graph();
        self.delete_element_quads(MetaKind::LinkType, id, domain)?;
        let _ = self.store.flush();
        Ok(())
    }

    // ── 阶段2：ActionType + FunctionDef 声明式 CRUD ─────────────────────────
    //
    // 复用阶段1的存储/备份/先删后插框架。动作与函数均为叶子元素（无下游引用），
    // 删除直接进行；upsert 校验 applies_to 引用的对象类型存在（防悬挂声明）。

    /// upsert ActionType（新建或整体替换）。校验 applies_to 对象存在；写前备份 meta 图。
    pub fn upsert_action_type(&self, domain: &str, action: &ActionType) -> Result<(), String> {
        let obj_ids = self.load_ids(MetaKind::ObjectType)?;
        if !obj_ids.iter().any(|o| o == &action.applies_to) {
            return Err(format!(
                "动作 applies_to 引用的对象类型不存在: {}",
                action.applies_to
            ));
        }
        let _ = self.backup_meta_graph();
        let value = serde_json::to_value(action).map_err(|e| e.to_string())?;
        self.write_element(
            MetaKind::ActionType,
            &action.id,
            &action.label,
            value,
            domain,
        )
    }

    /// 删除 ActionType（动作无下游引用，直接删）。写前备份。
    pub fn delete_action_type(&self, domain: &str, id: &str) -> Result<(), String> {
        let _ = self.backup_meta_graph();
        self.delete_element_quads(MetaKind::ActionType, id, domain)?;
        let _ = self.store.flush();
        Ok(())
    }

    /// upsert FunctionDef（新建或整体替换）。校验 applies_to 对象存在；写前备份 meta 图。
    pub fn upsert_function_def(&self, domain: &str, func: &FunctionDef) -> Result<(), String> {
        let obj_ids = self.load_ids(MetaKind::ObjectType)?;
        if !obj_ids.iter().any(|o| o == &func.applies_to) {
            return Err(format!(
                "函数 applies_to 引用的对象类型不存在: {}",
                func.applies_to
            ));
        }
        let _ = self.backup_meta_graph();
        let value = serde_json::to_value(func).map_err(|e| e.to_string())?;
        self.write_element(MetaKind::FunctionDef, &func.id, &func.label, value, domain)
    }

    /// 删除 FunctionDef（函数无下游引用，直接删）。写前备份。
    pub fn delete_function_def(&self, domain: &str, id: &str) -> Result<(), String> {
        let _ = self.backup_meta_graph();
        self.delete_element_quads(MetaKind::FunctionDef, id, domain)?;
        let _ = self.store.flush();
        Ok(())
    }
}

/// 构造 `<s> <p> <o> .` 三元组（对象为 IRI）。
fn fmt_iri(s: &str, p: &str, o: &str) -> String {
    format!("<{}> <{}> <{}> .", s, p, o)
}

/// 构造 `<s> <p> "lit" .` 三元组（对象为字符串字面量）。
fn fmt_lit(s: &str, p: &str, lit: &str) -> String {
    format!(
        "<{}> <{}> \"{}\" .",
        s,
        p,
        OntologyStore::escape_literal(lit)
    )
}

/// 构造 `<s> <p> "n"^^xsd:integer .` 三元组（对象为整数字面量）。
fn fmt_int(s: &str, p: &str, n: i64) -> String {
    format!(
        "<{}> <{}> \"{}\"^^<http://www.w3.org/2001/XMLSchema#integer> .",
        s, p, n
    )
}

/// domain 名转 IRI 安全片段。
fn iri_frag(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect()
}

/// 从 oxigraph Term 的字符串表示中剥离字面量引号与类型/语言标注，还原原始字符串值。
fn strip_literal(s: &str) -> String {
    if s.starts_with('"') {
        if let Some(pos) = s.rfind("\"^^<") {
            return unescape_literal(&s[1..pos]);
        }
        if let Some(pos) = s.rfind("\"@") {
            return unescape_literal(&s[1..pos]);
        }
        if s.ends_with('"') && s.len() > 1 {
            return unescape_literal(&s[1..s.len() - 1]);
        }
    }
    s.to_string()
}

/// 还原 SPARQL/Turtle 字面量转义（与 escape_literal 逆操作，覆盖 seed 写入的转义）。
fn unescape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seed_idempotent() {
        let store = OntologyStore::new().unwrap();
        assert!(store.ensure_seeded("ev-repair").unwrap(), "首次应写入");
        assert!(
            !store.ensure_seeded("ev-repair").unwrap(),
            "第二次应幂等跳过"
        );
    }

    #[test]
    fn test_load_roundtrip_equals_seed() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        let loaded = store.load_definition("ev-repair").unwrap();
        let seed = ev_repair_ontology();

        assert_eq!(loaded.object_types.len(), seed.object_types.len());
        assert_eq!(loaded.link_types.len(), seed.link_types.len());
        assert_eq!(loaded.action_types.len(), seed.action_types.len());
        assert_eq!(loaded.functions.len(), seed.functions.len());

        // 逐字段一致：序列化后比对（顺序 + 内容）。
        let a = serde_json::to_value(&loaded).unwrap();
        let b = serde_json::to_value(&seed).unwrap();
        assert_eq!(a, b, "load 往返应与 seed 逐字段一致");
    }

    #[test]
    fn test_domain_guardrails_roundtrip() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        let guardrails = ActionGuardrailConfig {
            max_triples: Some(12),
            allowed_predicate_prefixes: Some(vec!["https://example.test/".into()]),
            assertions: vec![],
        };
        store
            .upsert_domain_guardrails("ev-repair", &guardrails)
            .unwrap();
        assert_eq!(
            store.load_definition("ev-repair").unwrap().guardrails,
            guardrails
        );
    }

    #[test]
    fn test_load_preserves_object_order() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        let loaded = store.load_definition("ev-repair").unwrap();
        let seed = ev_repair_ontology();
        let loaded_ids: Vec<&str> = loaded.object_types.iter().map(|o| o.id.as_str()).collect();
        let seed_ids: Vec<&str> = seed.object_types.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(loaded_ids, seed_ids, "对象类型顺序应保持一致");
    }

    fn sample_obj(id: &str) -> ObjectType {
        ObjectType {
            id: id.into(),
            iri: super::super::ontology_layer::ev(id),
            label: format!("{}标签", id),
            description: "测试对象".into(),
            icon: "Box".into(),
            color: "blue".into(),
            primary_key: "name".into(),
            title_property: "name".into(),
            kind: Default::default(),
            properties: vec![],
        }
    }

    #[test]
    fn test_upsert_and_update_object_type() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        let n0 = store.load_ids(MetaKind::ObjectType).unwrap().len();

        // 新建
        store
            .upsert_object_type("ev-repair", &sample_obj("TestGizmo"))
            .unwrap();
        let n1 = store.load_ids(MetaKind::ObjectType).unwrap().len();
        assert_eq!(n1, n0 + 1, "upsert 新建应 +1");

        // 更新（同 id 幂等，不新增）
        let mut updated = sample_obj("TestGizmo");
        updated.label = "改名后".into();
        store.upsert_object_type("ev-repair", &updated).unwrap();
        let n2 = store.load_ids(MetaKind::ObjectType).unwrap().len();
        assert_eq!(n2, n1, "同 id upsert 应替换而非新增");
        let loaded = store.load_definition("ev-repair").unwrap();
        let got = loaded
            .object_types
            .iter()
            .find(|o| o.id == "TestGizmo")
            .unwrap();
        assert_eq!(got.label, "改名后");
    }

    #[test]
    fn test_delete_object_type_blocked_by_reference() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        // 新增对象 A、B 与一条 A→B 链接，再删 A 应被拦截。
        store
            .upsert_object_type("ev-repair", &sample_obj("Aaa"))
            .unwrap();
        store
            .upsert_object_type("ev-repair", &sample_obj("Bbb"))
            .unwrap();
        let link = LinkType {
            id: "AaaToBbb".into(),
            iri: super::super::ontology_layer::ev("AaaToBbb"),
            label: "关联".into(),
            description: "".into(),
            source: "Aaa".into(),
            target: "Bbb".into(),
            cardinality: super::super::ontology_layer::Cardinality::OneToMany,
        };
        store.upsert_link_type("ev-repair", &link).unwrap();

        let err = store.delete_object_type("ev-repair", "Aaa").unwrap_err();
        assert!(!err.is_empty(), "被链接引用应拒绝删除");

        // 删链接后可删对象。
        store.delete_link_type("ev-repair", "AaaToBbb").unwrap();
        store.delete_object_type("ev-repair", "Aaa").unwrap();
    }

    #[test]
    fn test_upsert_link_missing_object_rejected() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        let link = LinkType {
            id: "Bad".into(),
            iri: super::super::ontology_layer::ev("Bad"),
            label: "坏链接".into(),
            description: "".into(),
            source: "NoSuchObj".into(),
            target: "AlsoNone".into(),
            cardinality: super::super::ontology_layer::Cardinality::OneToOne,
        };
        assert!(
            store.upsert_link_type("ev-repair", &link).is_err(),
            "引用不存在对象应拒绝"
        );
    }

    fn sample_action(id: &str, applies_to: &str) -> ActionType {
        ActionType {
            id: id.into(),
            iri: super::super::ontology_layer::ev(&format!("action/{}", id)),
            label: format!("{}动作", id),
            description: "测试动作".into(),
            applies_to: applies_to.into(),
            parameters: vec![],
            preconditions: vec![],
            side_effects: vec![],
            icon: "Zap".into(),
            guardrails: ActionGuardrailConfig::default(),
        }
    }

    fn sample_function(id: &str, applies_to: &str) -> FunctionDef {
        FunctionDef {
            id: id.into(),
            label: format!("{}函数", id),
            description: "测试函数".into(),
            applies_to: applies_to.into(),
            returns: super::super::ontology_layer::PropertyType::Number,
            expression: "1 + 1".into(),
        }
    }

    #[test]
    fn test_upsert_and_update_action_type() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        let n0 = store.load_ids(MetaKind::ActionType).unwrap().len();

        // 新建（applies_to 为已 seed 的 FaultCode）。
        store
            .upsert_action_type("ev-repair", &sample_action("TestAct", "FaultCode"))
            .unwrap();
        let n1 = store.load_ids(MetaKind::ActionType).unwrap().len();
        assert_eq!(n1, n0 + 1, "upsert 新建应 +1");

        // 更新同 id 幂等替换。
        let mut updated = sample_action("TestAct", "FaultCode");
        updated.label = "改名后".into();
        store.upsert_action_type("ev-repair", &updated).unwrap();
        let n2 = store.load_ids(MetaKind::ActionType).unwrap().len();
        assert_eq!(n2, n1, "同 id upsert 应替换而非新增");
        let loaded = store.load_definition("ev-repair").unwrap();
        let got = loaded
            .action_types
            .iter()
            .find(|a| a.id == "TestAct")
            .unwrap();
        assert_eq!(got.label, "改名后");

        // 删除。
        store.delete_action_type("ev-repair", "TestAct").unwrap();
        let n3 = store.load_ids(MetaKind::ActionType).unwrap().len();
        assert_eq!(n3, n0, "删除应回到初始计数");
    }

    #[test]
    fn test_upsert_action_missing_applies_to_rejected() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        assert!(
            store
                .upsert_action_type("ev-repair", &sample_action("Bad", "NoSuchObj"))
                .is_err(),
            "applies_to 不存在对象应拒绝"
        );
    }

    #[test]
    fn test_upsert_and_update_function_def() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        let n0 = store.load_ids(MetaKind::FunctionDef).unwrap().len();

        store
            .upsert_function_def("ev-repair", &sample_function("TestFn", "FaultCode"))
            .unwrap();
        let n1 = store.load_ids(MetaKind::FunctionDef).unwrap().len();
        assert_eq!(n1, n0 + 1, "upsert 新建应 +1");

        let mut updated = sample_function("TestFn", "FaultCode");
        updated.expression = "2 + 2".into();
        store.upsert_function_def("ev-repair", &updated).unwrap();
        let loaded = store.load_definition("ev-repair").unwrap();
        let got = loaded.functions.iter().find(|f| f.id == "TestFn").unwrap();
        assert_eq!(got.expression, "2 + 2");

        store.delete_function_def("ev-repair", "TestFn").unwrap();
        let n3 = store.load_ids(MetaKind::FunctionDef).unwrap().len();
        assert_eq!(n3, n0, "删除应回到初始计数");
    }

    #[test]
    fn test_upsert_function_missing_applies_to_rejected() {
        let store = OntologyStore::new().unwrap();
        store.ensure_seeded("ev-repair").unwrap();
        assert!(
            store
                .upsert_function_def("ev-repair", &sample_function("Bad", "NoSuchObj"))
                .is_err(),
            "applies_to 不存在对象应拒绝"
        );
    }
}
