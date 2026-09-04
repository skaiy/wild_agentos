//! 本体层（Ontology Layer）——在现有 RDF/Oxigraph 三元组存储之上叠加的元模型。
//!
//! 受 Palantir Ontology 启发，将"裸三元组 + 词汇表"升级为可被业务直接理解、
//! 可执行的数字孪生模型。包含五大组件：
//!   - ObjectType   语义层·对象类型（属性 schema + 主键 + RDF class 映射）
//!   - LinkType     语义层·链接类型（方向 + 基数）
//!   - ActionType   动力层·操作类型（参数 schema + 前置条件 + side-effect 写回，让图谱"可写可执行"）
//!   - FunctionDef  动力层·函数（派生属性 / AI 计算）
//!   - KnowledgePack 知识包·业务知识的封装与隔离单元（独立命名图 + 向量命名空间）
//!
//! Phase 1 为只读种子定义，不改动底层存储。

use serde::{Deserialize, Serialize};

/// 本体域 IRI 前缀（新能源车维修域）。
pub fn ev(s: &str) -> String {
    format!("https://agentos.ontology/ev/{}", s)
}

/// 属性数据类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyType {
    String,
    Text,
    Integer,
    Number,
    Boolean,
    DateTime,
    Enum,
}

/// 对象数据归属：知识（沉淀于图谱）vs 业务（图谱外，未来经 MCP 对接业务库查询）。
/// 默认 Knowledge，保证既有已序列化数据反序列化时向后兼容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    #[default]
    Knowledge,
    Business,
}

/// 对象类型的属性规格。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertySpec {
    pub name: String,
    pub label: String,
    pub prop_type: PropertyType,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enum_values: Vec<String>,
}

impl PropertySpec {
    pub fn new(name: &str, label: &str, prop_type: PropertyType) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            prop_type,
            required: false,
            description: None,
            enum_values: Vec::new(),
        }
    }
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
    pub fn enums(mut self, vals: &[&str]) -> Self {
        self.enum_values = vals.iter().map(|s| s.to_string()).collect();
        self
    }
}

/// ObjectType — 语义层对象类型（带属性 schema 与主键）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectType {
    pub id: String,
    pub iri: String,
    pub label: String,
    pub description: String,
    pub icon: String,
    pub color: String,
    pub primary_key: String,
    pub title_property: String,
    /// 数据归属（知识/业务）。业务对象不入图谱，未来经 MCP 对接业务库。
    #[serde(default)]
    pub kind: ObjectKind,
    pub properties: Vec<PropertySpec>,
}

impl ObjectType {
    /// 标记为业务对象（实例数据归业务库，未来经 MCP 查询）。
    pub fn business(mut self) -> Self {
        self.kind = ObjectKind::Business;
        self
    }
}

/// 按对象类型 id 返回其数据归属（未知类型默认 Knowledge）。
pub fn object_kind_of(id: &str) -> ObjectKind {
    ev_repair_ontology()
        .object_types
        .iter()
        .find(|o| o.id == id)
        .map(|o| o.kind)
        .unwrap_or_default()
}

/// 链接基数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    OneToOne,
    OneToMany,
    ManyToOne,
    ManyToMany,
}

/// LinkType — 语义层链接类型（带方向与基数）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkType {
    pub id: String,
    pub iri: String,
    pub label: String,
    pub description: String,
    pub source: String,
    pub target: String,
    pub cardinality: Cardinality,
}

/// Action 参数规格。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionParam {
    pub name: String,
    pub label: String,
    pub prop_type: PropertyType,
    #[serde(default)]
    pub required: bool,
}

impl ActionParam {
    pub fn new(name: &str, label: &str, prop_type: PropertyType, required: bool) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            prop_type,
            required,
        }
    }
}

/// side-effect（写回图谱）类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectKind {
    CreateObject,
    UpdateProperty,
    CreateLink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideEffect {
    pub kind: SideEffectKind,
    pub target_object_type: String,
    pub description: String,
}

/// 一条轻量级 SPARQL ASK 护栏断言。查询返回 true 时表示违反该约束。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SparqlAskAssertion {
    /// 稳定、可读的违规代码，供 API 调用方和审计系统识别。
    pub code: String,
    /// 仅允许 ASK 查询；执行时由服务端绑定到 JWT claims 派生的影子图。
    pub query: String,
}

/// 动作写回数据沙箱的可选配置。
///
/// `None` 不代表关闭护栏，而是继承域配置；域配置也缺失时使用安全的内置默认值。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionGuardrailConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_triples: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_predicate_prefixes: Option<Vec<String>>,
    #[serde(default)]
    pub assertions: Vec<SparqlAskAssertion>,
}

/// ActionType — 动力层操作类型（让图谱从"只读"变"可写可执行"）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionType {
    pub id: String,
    pub iri: String,
    pub label: String,
    pub description: String,
    pub applies_to: String,
    pub parameters: Vec<ActionParam>,
    pub preconditions: Vec<String>,
    pub side_effects: Vec<SideEffect>,
    pub icon: String,
    /// 动作级护栏覆盖；未配置的项继承域默认值，不能用空值关闭内置护栏。
    #[serde(default)]
    pub guardrails: ActionGuardrailConfig,
}

/// FunctionDef — 动力层函数（派生属性 / AI 计算）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub id: String,
    pub label: String,
    pub description: String,
    pub applies_to: String,
    pub returns: PropertyType,
    pub expression: String,
}

/// 一个业务域的完整本体定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OntologyDefinition {
    pub domain: String,
    /// 此业务域的护栏默认值；动作级设置可逐项覆盖。
    #[serde(default)]
    pub guardrails: ActionGuardrailConfig,
    pub object_types: Vec<ObjectType>,
    pub link_types: Vec<LinkType>,
    pub action_types: Vec<ActionType>,
    pub functions: Vec<FunctionDef>,
}

/// 知识包统计摘要。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgePackStats {
    pub object_types: usize,
    pub link_types: usize,
    pub action_types: usize,
    pub functions: usize,
}

/// KnowledgePack — 业务知识的封装与隔离单元。
///
/// 每个知识包拥有独立的 RDF 命名图与向量命名空间，实现包间隔离；
/// Agent 可挂载多个知识包。未来可呈现多种业务知识包形态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgePack {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub icon: String,
    pub color: String,
    /// RDF 命名图（隔离单元）。
    pub named_graph: String,
    /// 向量切片命名空间（隔离单元）。
    pub vector_namespace: String,
    /// 本体域 IRI 前缀。
    pub ontology_domain: String,
    pub stats: KnowledgePackStats,
    /// 关联的知识库分类 id（1:N）。
    #[serde(default)]
    pub category_ids: Vec<String>,
    /// 关联的图知识库 id（1:N）。
    #[serde(default)]
    pub graph_kb_ids: Vec<String>,
    /// 关联的向量知识库 id（1:N）。
    #[serde(default)]
    pub vector_kb_ids: Vec<String>,
    /// 是否可编辑（内置包亦可编辑，标记用于前端提示）。
    #[serde(default)]
    pub builtin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

// ─── 种子构造辅助 ──────────────────────────────────────────

fn ps(name: &str, label: &str, pt: PropertyType) -> PropertySpec {
    PropertySpec::new(name, label, pt)
}

#[allow(clippy::too_many_arguments)]
fn obj(
    id: &str,
    label: &str,
    desc: &str,
    icon: &str,
    color: &str,
    pk: &str,
    title: &str,
    props: Vec<PropertySpec>,
) -> ObjectType {
    ObjectType {
        id: id.into(),
        iri: ev(id),
        label: label.into(),
        description: desc.into(),
        icon: icon.into(),
        color: color.into(),
        primary_key: pk.into(),
        title_property: title.into(),
        kind: ObjectKind::Knowledge,
        properties: props,
    }
}

fn link(
    id: &str,
    label: &str,
    desc: &str,
    source: &str,
    target: &str,
    card: Cardinality,
) -> LinkType {
    LinkType {
        id: id.into(),
        iri: ev(id),
        label: label.into(),
        description: desc.into(),
        source: source.into(),
        target: target.into(),
        cardinality: card,
    }
}

/// 新能源车维修域本体定义（语义层 + 动力层）。
pub fn ev_repair_ontology() -> OntologyDefinition {
    use Cardinality::*;
    use PropertyType::{Boolean, DateTime, Enum, Integer, Number, String as PStr, Text};

    let object_types = vec![
        obj(
            "Brand",
            "品牌",
            "新能源车整车品牌",
            "Building2",
            "blue",
            "name",
            "name",
            vec![
                ps("name", "品牌名称", PStr).required(),
                ps("country", "产地国", PStr),
                ps("logo_url", "标识图", PStr),
            ],
        ),
        obj(
            "VehicleModel",
            "车型",
            "某品牌下的具体车型/年款",
            "Car",
            "indigo",
            "model_id",
            "name",
            vec![
                ps("model_id", "车型ID", PStr).required(),
                ps("name", "车型名称", PStr).required(),
                ps("brand", "所属品牌", PStr),
                ps("year", "年款", Integer),
                ps("powertrain", "动力类型", Enum).enums(&["纯电EV", "插电PHEV", "混动HEV"]),
                ps("battery_capacity", "电池容量(kWh)", Number),
            ],
        ),
        obj(
            "Vehicle",
            "车辆",
            "在用的具体车辆实例",
            "CarFront",
            "slate",
            "vin",
            "vin",
            vec![
                ps("vin", "车架号VIN", PStr).required(),
                ps("model", "车型", PStr),
                ps("mileage", "里程(km)", Integer),
                ps("plate", "车牌号", PStr),
                ps("production_date", "出厂日期", DateTime),
            ],
        )
        .business(),
        obj(
            "System",
            "系统",
            "整车功能系统",
            "Cpu",
            "violet",
            "system_id",
            "name",
            vec![
                ps("system_id", "系统ID", PStr).required(),
                ps("name", "系统名称", PStr).required(),
                ps("category", "系统类别", Enum).enums(&[
                    "动力电池",
                    "电驱",
                    "电控",
                    "充电",
                    "热管理",
                    "整车控制",
                ]),
            ],
        ),
        obj(
            "FaultCode",
            "故障码",
            "诊断故障码(DTC)",
            "AlertTriangle",
            "amber",
            "code",
            "title",
            vec![
                ps("code", "故障码", PStr).required(),
                ps("title", "故障名称", PStr).required(),
                ps("standard", "标准来源", Enum).enums(&["SAE J2012", "厂家自定义"]),
                ps("meaning", "含义", Text),
                ps("can_drive", "能否继续行驶", Boolean),
                ps("description", "详细描述", Text),
            ],
        ),
        obj(
            "SeverityLevel",
            "严重等级",
            "故障严重程度分级",
            "Gauge",
            "rose",
            "level",
            "level",
            vec![
                ps("level", "等级", Enum)
                    .required()
                    .enums(&["提示", "轻微", "警告", "严重", "危险"]),
                ps("action_required", "建议响应", Text),
                ps("color_code", "色标", PStr),
            ],
        ),
        obj(
            "Cause",
            "原因",
            "故障的可能/常见原因",
            "Search",
            "orange",
            "cause_id",
            "description",
            vec![
                ps("cause_id", "原因ID", PStr).required(),
                ps("description", "原因描述", Text).required(),
                ps("likelihood", "可能性", Enum).enums(&["常见", "可能", "少见"]),
                ps("category", "原因类别", PStr),
            ],
        ),
        obj(
            "DiagnosticStep",
            "诊断排查步骤",
            "定位故障的排查步骤",
            "ListChecks",
            "cyan",
            "step_id",
            "instruction",
            vec![
                ps("step_id", "步骤ID", PStr).required(),
                ps("order", "序号", Integer),
                ps("instruction", "操作说明", Text).required(),
                ps("expected", "预期结果", Text),
                ps("tool_required", "所需工具", PStr),
            ],
        ),
        obj(
            "HandlingMeasure",
            "处理措施",
            "报警后车主/技师应采取的措施",
            "Wrench",
            "emerald",
            "measure_id",
            "title",
            vec![
                ps("measure_id", "措施ID", PStr).required(),
                ps("title", "措施标题", PStr).required(),
                ps("audience", "面向对象", Enum).enums(&["车主", "维修技师"]),
                ps("instruction", "处理说明", Text).required(),
                ps("urgency", "紧急程度", Enum).enums(&["立即", "尽快", "常规"]),
            ],
        ),
        obj(
            "FAQ",
            "常见问答",
            "围绕故障的常见问答",
            "MessageCircleQuestion",
            "teal",
            "faq_id",
            "question",
            vec![
                ps("faq_id", "问答ID", PStr).required(),
                ps("question", "问题", Text).required(),
                ps("answer", "回答", Text).required(),
            ],
        ),
        obj(
            "CostReference",
            "费用参考",
            "维修/更换费用区间",
            "Banknote",
            "lime",
            "cost_id",
            "item",
            vec![
                ps("cost_id", "费用ID", PStr).required(),
                ps("item", "费用项目", PStr).required(),
                ps("min_price", "最低价(元)", Number),
                ps("max_price", "最高价(元)", Number),
                ps("currency", "币种", PStr),
                ps("note", "备注", Text),
            ],
        ),
        obj(
            "TechnicalInfo",
            "技术信息",
            "技术资料/规格参数",
            "FileText",
            "sky",
            "tech_id",
            "title",
            vec![
                ps("tech_id", "技术信息ID", PStr).required(),
                ps("title", "标题", PStr).required(),
                ps("content", "技术内容", Text).required(),
                ps("spec", "规格参数", PStr),
            ],
        ),
        obj(
            "DataSource",
            "数据来源",
            "知识的来源与可信度溯源",
            "Database",
            "zinc",
            "source_id",
            "name",
            vec![
                ps("source_id", "来源ID", PStr).required(),
                ps("name", "来源名称", PStr).required(),
                ps("url", "来源URL", PStr),
                ps("source_type", "来源类型", Enum).enums(&[
                    "官方手册",
                    "行业标准",
                    "实测数据",
                    "第三方平台",
                ]),
                ps("credibility", "可信度", Enum).enums(&["高", "中", "低"]),
                ps("captured_at", "采集时间", DateTime),
            ],
        ),
        obj(
            "Battery",
            "动力电池",
            "高压动力电池包",
            "BatteryCharging",
            "green",
            "battery_id",
            "battery_id",
            vec![
                ps("battery_id", "电池ID", PStr).required(),
                ps("chemistry", "电芯类型", Enum).enums(&["磷酸铁锂", "三元锂", "钠离子"]),
                ps("capacity_kwh", "容量(kWh)", Number),
                ps("soh", "健康度SOH(%)", Number),
                ps("cycle_count", "循环次数", Integer),
                ps("warranty_years", "质保年限", Integer),
            ],
        )
        .business(),
        obj(
            "RepairOrder",
            "维修工单",
            "由诊断动作生成的可执行工单",
            "ClipboardList",
            "purple",
            "order_id",
            "order_id",
            vec![
                ps("order_id", "工单号", PStr).required(),
                ps("vehicle_vin", "车架号", PStr).required(),
                ps("fault_code", "关联故障码", PStr),
                ps("status", "状态", Enum).enums(&["待处理", "处理中", "已完成", "已取消"]),
                ps("created_at", "创建时间", DateTime),
                ps("estimated_cost", "预估费用(元)", Number),
                ps("assigned_to", "指派技师", PStr),
            ],
        )
        .business(),
    ];

    let link_types = vec![
        link(
            "hasModel",
            "拥有车型",
            "品牌下的车型",
            "Brand",
            "VehicleModel",
            OneToMany,
        ),
        link(
            "hasSystem",
            "包含系统",
            "车型包含的功能系统",
            "VehicleModel",
            "System",
            OneToMany,
        ),
        link(
            "hasVehicle",
            "实例车辆",
            "车型对应的在用车辆",
            "VehicleModel",
            "Vehicle",
            OneToMany,
        ),
        link(
            "hasBattery",
            "搭载电池",
            "车辆搭载的动力电池",
            "Vehicle",
            "Battery",
            OneToOne,
        ),
        link(
            "triggers",
            "触发故障",
            "车辆触发的故障码",
            "Vehicle",
            "FaultCode",
            OneToMany,
        ),
        link(
            "affectsSystem",
            "影响系统",
            "故障码影响的系统",
            "FaultCode",
            "System",
            ManyToOne,
        ),
        link(
            "hasSeverity",
            "严重等级",
            "故障码的严重等级",
            "FaultCode",
            "SeverityLevel",
            ManyToOne,
        ),
        link(
            "hasCause",
            "可能原因",
            "故障码的可能/常见原因",
            "FaultCode",
            "Cause",
            OneToMany,
        ),
        link(
            "hasDiagnosticStep",
            "诊断步骤",
            "故障码的排查步骤",
            "FaultCode",
            "DiagnosticStep",
            OneToMany,
        ),
        link(
            "recommends",
            "建议措施",
            "故障码建议的处理措施",
            "FaultCode",
            "HandlingMeasure",
            OneToMany,
        ),
        link(
            "relatedFaq",
            "相关问答",
            "故障码相关的FAQ",
            "FaultCode",
            "FAQ",
            OneToMany,
        ),
        link(
            "hasCostReference",
            "费用参考",
            "故障码的费用参考",
            "FaultCode",
            "CostReference",
            OneToMany,
        ),
        link(
            "hasTechnicalInfo",
            "技术信息",
            "故障码的技术资料",
            "FaultCode",
            "TechnicalInfo",
            OneToMany,
        ),
        link(
            "sourcedFrom",
            "数据来源",
            "知识的来源溯源",
            "FaultCode",
            "DataSource",
            ManyToOne,
        ),
        link(
            "forVehicle",
            "服务车辆",
            "工单服务的车辆",
            "RepairOrder",
            "Vehicle",
            ManyToOne,
        ),
        link(
            "diagnoses",
            "诊断故障",
            "工单诊断的故障码",
            "RepairOrder",
            "FaultCode",
            ManyToOne,
        ),
        link(
            "applies",
            "采用措施",
            "工单采用的处理措施",
            "RepairOrder",
            "HandlingMeasure",
            ManyToMany,
        ),
    ];

    let action_types = vec![
        ActionType {
            id: "GenerateRepairOrder".into(),
            iri: ev("action/GenerateRepairOrder"),
            label: "生成维修工单".into(),
            description:
                "依据已确诊的故障码，为指定车辆创建可执行维修工单并建立关联（让图谱从只读变可写）。"
                    .into(),
            applies_to: "FaultCode".into(),
            parameters: vec![
                ActionParam::new("vehicle_vin", "车架号VIN", PropertyType::String, true),
                ActionParam::new("assigned_to", "指派技师", PropertyType::String, false),
                ActionParam::new(
                    "estimated_cost",
                    "预估费用(元)",
                    PropertyType::Number,
                    false,
                ),
            ],
            preconditions: vec!["车辆VIN已存在于图谱".into(), "故障码已确诊".into()],
            side_effects: vec![
                SideEffect {
                    kind: SideEffectKind::CreateObject,
                    target_object_type: "RepairOrder".into(),
                    description: "新建维修工单对象".into(),
                },
                SideEffect {
                    kind: SideEffectKind::CreateLink,
                    target_object_type: "Vehicle".into(),
                    description: "RepairOrder -forVehicle-> Vehicle".into(),
                },
                SideEffect {
                    kind: SideEffectKind::CreateLink,
                    target_object_type: "FaultCode".into(),
                    description: "RepairOrder -diagnoses-> FaultCode".into(),
                },
            ],
            icon: "ClipboardList".into(),
            guardrails: ActionGuardrailConfig::default(),
        },
        ActionType {
            id: "UpdateBatterySoh".into(),
            iri: ev("action/UpdateBatterySoh"),
            label: "更新电池健康度(SOH)".into(),
            description: "检测后写回电池 SOH 值，驱动健康度评分与质保判定。".into(),
            applies_to: "Battery".into(),
            parameters: vec![
                ActionParam::new("battery_id", "电池ID", PropertyType::String, true),
                ActionParam::new("soh", "健康度SOH(%)", PropertyType::Number, true),
            ],
            preconditions: vec!["电池对象已存在".into(), "SOH 取值范围 0-100".into()],
            side_effects: vec![SideEffect {
                kind: SideEffectKind::UpdateProperty,
                target_object_type: "Battery".into(),
                description: "更新 Battery.soh 属性".into(),
            }],
            icon: "BatteryCharging".into(),
            guardrails: ActionGuardrailConfig::default(),
        },
        ActionType {
            id: "MarkRecall".into(),
            iri: ev("action/MarkRecall"),
            label: "标记召回".into(),
            description: "对存在批次性缺陷的车型打召回标记，触发后续车主通知流程。".into(),
            applies_to: "VehicleModel".into(),
            parameters: vec![
                ActionParam::new("model_id", "车型ID", PropertyType::String, true),
                ActionParam::new("recall_reason", "召回原因", PropertyType::Text, true),
            ],
            preconditions: vec!["车型对象已存在".into()],
            side_effects: vec![SideEffect {
                kind: SideEffectKind::UpdateProperty,
                target_object_type: "VehicleModel".into(),
                description: "写入召回标记与原因".into(),
            }],
            icon: "Megaphone".into(),
            guardrails: ActionGuardrailConfig::default(),
        },
        ActionType {
            id: "AppendFaq".into(),
            iri: ev("action/AppendFaq"),
            label: "追加常见问答".into(),
            description: "将一次诊断沉淀为可复用的 FAQ，挂接到对应故障码。".into(),
            applies_to: "FaultCode".into(),
            parameters: vec![
                ActionParam::new("code", "故障码", PropertyType::String, true),
                ActionParam::new("question", "问题", PropertyType::Text, true),
                ActionParam::new("answer", "回答", PropertyType::Text, true),
            ],
            preconditions: vec!["故障码对象已存在".into()],
            side_effects: vec![
                SideEffect {
                    kind: SideEffectKind::CreateObject,
                    target_object_type: "FAQ".into(),
                    description: "新建 FAQ 对象".into(),
                },
                SideEffect {
                    kind: SideEffectKind::CreateLink,
                    target_object_type: "FaultCode".into(),
                    description: "FaultCode -relatedFaq-> FAQ".into(),
                },
            ],
            icon: "MessageCirclePlus".into(),
            guardrails: ActionGuardrailConfig::default(),
        },
    ];

    let functions = vec![
        FunctionDef {
            id: "BatteryHealthScore".into(),
            label: "电池健康度评分".into(),
            description: "综合 SOH、循环次数与质保年限计算电池健康度评分(0-100)。".into(),
            applies_to: "Battery".into(),
            returns: PropertyType::Number,
            expression: "score = clamp(soh*0.7 + (1 - cycle_count/3000)*30, 0, 100)".into(),
        },
        FunctionDef {
            id: "FaultRiskLevel".into(),
            label: "故障风险等级".into(),
            description: "依据严重等级与能否行驶推导风险等级。".into(),
            applies_to: "FaultCode".into(),
            returns: PropertyType::Enum,
            expression: "risk = map(severity, can_drive) -> {低,中,高,紧急}".into(),
        },
        FunctionDef {
            id: "RepairCostEstimate".into(),
            label: "维修费用预估".into(),
            description: "汇总关联处理措施的费用参考上限，给出预估费用。".into(),
            applies_to: "FaultCode".into(),
            returns: PropertyType::Number,
            expression: "estimate = sum(CostReference.max_price for measure in recommends)".into(),
        },
    ];

    OntologyDefinition {
        domain: "ev-repair".into(),
        guardrails: ActionGuardrailConfig::default(),
        object_types,
        link_types,
        action_types,
        functions,
    }
}

/// 系统内置的知识包清单（Phase 1 仅含新能源车维修故障库）。
pub fn knowledge_packs() -> Vec<KnowledgePack> {
    let ont = ev_repair_ontology();
    let stats = KnowledgePackStats {
        object_types: ont.object_types.len(),
        link_types: ont.link_types.len(),
        action_types: ont.action_types.len(),
        functions: ont.functions.len(),
    };
    vec![KnowledgePack {
        id: "ev-repair-fault-kb".into(),
        name: "新能源车维修故障库".into(),
        description: "覆盖品牌/车型/系统/故障码/原因/诊断步骤/处理措施/费用参考/FAQ/数据来源的新能源车维修知识包，封装知识图谱与向量切片，支持 Agent 挂载与包间隔离。".into(),
        version: "1.0.0".into(),
        icon: "BatteryCharging".into(),
        color: "emerald".into(),
        named_graph: "graph:pack/ev-repair".into(),
        vector_namespace: "vec:pack/ev-repair".into(),
        ontology_domain: "https://agentos.ontology/ev/".into(),
        stats,
        category_ids: Vec::new(),
        graph_kb_ids: Vec::new(),
        vector_kb_ids: Vec::new(),
        builtin: true,
        created_at: None,
        updated_at: None,
    }]
}
