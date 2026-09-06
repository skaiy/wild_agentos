//! Conservative adapters that turn tabular/schema inputs into reviewable
//! ontology type drafts. They deliberately never create ActionTypes.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ontology_layer::{
    ev, Cardinality, LinkType, ObjectKind, ObjectType, PropertySpec, PropertyType,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeDraftBundle {
    pub object_types: Vec<ObjectType>,
    pub link_types: Vec<LinkType>,
    /// Relationship candidates kept outside the promotable bundle. A caller
    /// must explicitly add one through the normal `links` input before it can
    /// reach the human promotion path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_links: Vec<DraftLinkInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftLinkInput {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: String,
    pub source: String,
    pub target: String,
    #[serde(default = "default_cardinality")]
    pub cardinality: Cardinality,
}

fn default_cardinality() -> Cardinality {
    Cardinality::OneToMany
}

impl From<DraftLinkInput> for LinkType {
    fn from(link: DraftLinkInput) -> Self {
        Self {
            iri: ev(&link.id),
            label: link.label.unwrap_or_else(|| link.id.clone()),
            id: link.id,
            description: link.description,
            source: link.source,
            target: link.target,
            cardinality: link.cardinality,
        }
    }
}

pub fn from_csv_headers(
    csv: &str,
    object_id: Option<&str>,
    label: Option<&str>,
    primary_key: Option<&str>,
    links: Vec<DraftLinkInput>,
) -> Result<TypeDraftBundle, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(csv.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| format!("invalid CSV headers: {e}"))?
        .iter()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(normalize_property_name)
        .collect::<Vec<_>>();
    if headers.is_empty() {
        return Err("CSV must contain at least one non-empty header".into());
    }
    if headers.iter().any(|name| name.is_empty()) {
        return Err("CSV header has no usable property name".into());
    }
    let id = object_id
        .map(normalize_type_id)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "CsvRecord".to_string());
    let key = primary_key
        .map(normalize_property_name)
        .filter(|key| headers.contains(key))
        .unwrap_or_else(|| choose_primary_key(&headers));
    Ok(TypeDraftBundle {
        object_types: vec![ObjectType {
            iri: ev(&id),
            label: label.unwrap_or(&id).to_string(),
            description: "Draft inferred from CSV column headers; review before promotion.".into(),
            icon: "Table".into(),
            color: "slate".into(),
            primary_key: key.clone(),
            title_property: key.clone(),
            kind: ObjectKind::Knowledge,
            properties: headers
                .iter()
                .map(|name| PropertySpec {
                    name: name.clone(),
                    label: name.clone(),
                    prop_type: PropertyType::String,
                    required: name == &key,
                    description: None,
                    enum_values: vec![],
                })
                .collect(),
            id,
        }],
        link_types: links.into_iter().map(Into::into).collect(),
        suggested_links: vec![],
        warnings: vec![
            "CSV draft properties default to string; verify types and required fields before promotion."
                .into(),
            "No ActionType is generated from CSV input.".into(),
        ],
    })
}

pub fn from_json_schema(
    schema: &Value,
    object_id: Option<&str>,
    label: Option<&str>,
    links: Vec<DraftLinkInput>,
) -> Result<TypeDraftBundle, String> {
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| "JSON Schema root must define an object `properties` map".to_string())?;
    let id = object_id
        .map(normalize_type_id)
        .filter(|id| !id.is_empty())
        .or_else(|| {
            schema
                .get("title")
                .and_then(Value::as_str)
                .map(normalize_type_id)
        })
        .unwrap_or_else(|| "SchemaObject".to_string());
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut props = properties
        .iter()
        .map(|(name, definition)| property_from_schema(name, definition, &required))
        .collect::<Vec<_>>();
    props.sort_by(|a, b| a.name.cmp(&b.name));
    if props.is_empty() {
        return Err("JSON Schema must define at least one property".into());
    }
    let key = props
        .iter()
        .find(|prop| prop.required && matches!(prop.prop_type, PropertyType::String))
        .or_else(|| props.iter().find(|prop| prop.required))
        .unwrap_or(&props[0])
        .name
        .clone();
    Ok(TypeDraftBundle {
        object_types: vec![ObjectType {
            iri: ev(&id),
            label: label
                .map(str::to_string)
                .or_else(|| {
                    schema
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| id.clone()),
            description: schema
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("Draft inferred from JSON Schema; review before promotion.")
                .to_string(),
            icon: "Braces".into(),
            color: "slate".into(),
            primary_key: key.clone(),
            title_property: key,
            kind: ObjectKind::Knowledge,
            properties: props,
            id,
        }],
        link_types: links.into_iter().map(Into::into).collect(),
        suggested_links: vec![],
        warnings: vec!["No ActionType is generated from JSON Schema input.".into()],
    })
}

/// Create object type drafts from the component schemas in an OpenAPI 3.x
/// document. Links are accepted only from explicit request input or the
/// document's `x-ontology-links` extension; ordinary `$ref` properties never
/// become relationships implicitly.
pub fn from_openapi(
    document: &Value,
    links: Vec<DraftLinkInput>,
) -> Result<TypeDraftBundle, String> {
    let version = document
        .get("openapi")
        .and_then(Value::as_str)
        .ok_or_else(|| "OpenAPI document must include an `openapi` version".to_string())?;
    if !version.starts_with('3') {
        return Err(format!("only OpenAPI 3.x is supported, got `{version}`"));
    }
    let schemas = document
        .pointer("/components/schemas")
        .and_then(Value::as_object)
        .ok_or_else(|| "OpenAPI document must define `components.schemas`".to_string())?;
    let mut object_types = schemas
        .iter()
        .filter_map(|(name, schema)| openapi_object_type(name, schema, schemas))
        .collect::<Vec<_>>();
    object_types.sort_by(|a, b| a.id.cmp(&b.id));
    if object_types.is_empty() {
        return Err("OpenAPI `components.schemas` must contain at least one object schema".into());
    }

    let mut explicit_links = document
        .get("x-ontology-links")
        .map(|value| {
            serde_json::from_value::<Vec<DraftLinkInput>>(value.clone())
                .map_err(|error| format!("invalid `x-ontology-links`: {error}"))
        })
        .transpose()?
        .unwrap_or_default();
    explicit_links.extend(links);
    Ok(TypeDraftBundle {
        object_types,
        link_types: explicit_links.into_iter().map(Into::into).collect(),
        suggested_links: vec![],
        warnings: vec![
            "OpenAPI relationship properties are not inferred as LinkTypes; only explicit x-ontology-links or request links are included."
                .into(),
            "No ActionType is generated from OpenAPI input.".into(),
        ],
    })
}

/// Create object and relationship drafts from a deliberately small SQL DDL
/// subset: `CREATE TABLE`, column declarations, primary keys, and explicit
/// foreign keys. Unsupported statements are ignored rather than guessed.
pub fn from_sql_ddl(ddl: &str, links: Vec<DraftLinkInput>) -> Result<TypeDraftBundle, String> {
    let tables = parse_create_tables(ddl)?;
    if tables.is_empty() {
        return Err("SQL DDL must contain at least one supported CREATE TABLE statement".into());
    }
    let mut object_types = Vec::with_capacity(tables.len());
    let mut foreign_keys = Vec::new();
    for table in &tables {
        let id = normalize_type_id(&table.name);
        let key = table
            .primary_key
            .as_deref()
            .map(normalize_property_name)
            .filter(|key| table.columns.iter().any(|column| column.name == *key))
            .unwrap_or_else(|| {
                choose_primary_key(
                    &table
                        .columns
                        .iter()
                        .map(|column| column.name.clone())
                        .collect::<Vec<_>>(),
                )
            });
        object_types.push(ObjectType {
            iri: ev(&id),
            label: table.name.clone(),
            description: format!(
                "Draft inferred from SQL DDL table `{}`; review before promotion.",
                table.name
            ),
            icon: "Table".into(),
            color: "slate".into(),
            primary_key: key.clone(),
            title_property: key.clone(),
            kind: ObjectKind::Knowledge,
            properties: table
                .columns
                .iter()
                .map(|column| PropertySpec {
                    name: column.name.clone(),
                    label: column.name.clone(),
                    prop_type: sql_property_type(&column.sql_type),
                    required: column.not_null || column.name == key,
                    description: None,
                    enum_values: vec![],
                })
                .collect(),
            id: id.clone(),
        });
        foreign_keys.extend(table.foreign_keys.iter().map(|foreign_key| DraftLinkInput {
            id: normalize_type_id(&format!(
                "{} {} to {}",
                table.name, foreign_key.column, foreign_key.target_table
            )),
            label: Some(format!("{} to {}", table.name, foreign_key.target_table)),
            description: format!(
                "Draft from explicit SQL foreign key {}.{} -> {}.{}.",
                table.name, foreign_key.column, foreign_key.target_table, foreign_key.target_column
            ),
            source: id.clone(),
            target: normalize_type_id(&foreign_key.target_table),
            cardinality: Cardinality::ManyToOne,
        }));
    }
    object_types.sort_by(|a, b| a.id.cmp(&b.id));
    foreign_keys.extend(links);
    Ok(TypeDraftBundle {
        object_types,
        link_types: foreign_keys.into_iter().map(Into::into).collect(),
        suggested_links: vec![],
        warnings: vec![
            "SQL relationship drafts come only from explicit FOREIGN KEY or REFERENCES clauses and caller-supplied links."
                .into(),
            "Only a CREATE TABLE SQL DDL subset is supported; review types, nullability, and keys before promotion."
                .into(),
            "No ActionType is generated from SQL DDL input.".into(),
        ],
    })
}

fn openapi_object_type(
    name: &str,
    schema: &Value,
    schemas: &serde_json::Map<String, Value>,
) -> Option<ObjectType> {
    let resolved = resolve_openapi_ref(schema, schemas);
    let properties = resolved.get("properties")?.as_object()?;
    let required = resolved
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut properties = properties
        .iter()
        .map(|(property, definition)| property_from_schema(property, definition, &required))
        .collect::<Vec<_>>();
    properties.sort_by(|a, b| a.name.cmp(&b.name));
    if properties.is_empty() {
        return None;
    }
    let id = normalize_type_id(name);
    let key = properties
        .iter()
        .find(|property| property.required && matches!(property.prop_type, PropertyType::String))
        .or_else(|| properties.iter().find(|property| property.required))
        .unwrap_or(&properties[0])
        .name
        .clone();
    Some(ObjectType {
        iri: ev(&id),
        label: resolved
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_string(),
        description: resolved
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("Draft inferred from OpenAPI; review before promotion.")
            .to_string(),
        icon: "Braces".into(),
        color: "slate".into(),
        primary_key: key.clone(),
        title_property: key,
        kind: ObjectKind::Knowledge,
        properties,
        id,
    })
}

fn resolve_openapi_ref<'a>(
    schema: &'a Value,
    schemas: &'a serde_json::Map<String, Value>,
) -> &'a Value {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix("#/components/schemas/"))
        .and_then(|name| schemas.get(name))
        .unwrap_or(schema)
}

#[derive(Debug)]
struct SqlTable {
    name: String,
    columns: Vec<SqlColumn>,
    primary_key: Option<String>,
    foreign_keys: Vec<SqlForeignKey>,
}

#[derive(Debug)]
struct SqlColumn {
    name: String,
    sql_type: String,
    not_null: bool,
}

#[derive(Debug)]
struct SqlForeignKey {
    column: String,
    target_table: String,
    target_column: String,
}

fn parse_create_tables(ddl: &str) -> Result<Vec<SqlTable>, String> {
    let create =
        Regex::new(r"(?i)CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\(")
            .expect("constant create table regex is valid");
    let foreign_key = Regex::new(
        r"(?i)(?:CONSTRAINT\s+[A-Za-z_][A-Za-z0-9_]*\s+)?FOREIGN\s+KEY\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)\s*REFERENCES\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)",
    )
    .expect("constant foreign key regex is valid");
    let inline_reference = Regex::new(
        r"(?i)^([A-Za-z_][A-Za-z0-9_]*)\s+([A-Za-z][A-Za-z0-9_]*)(?:\s*\([^)]*\))?.*\bREFERENCES\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)",
    )
    .expect("constant inline reference regex is valid");
    let mut tables = Vec::new();
    for matches in create.captures_iter(ddl) {
        let whole = matches.get(0).expect("full match exists");
        let Some(body) = balanced_sql_body(&ddl[whole.end()..]) else {
            return Err(format!("unterminated CREATE TABLE `{}`", &matches[1]));
        };
        let mut table = SqlTable {
            name: matches[1].to_string(),
            columns: vec![],
            primary_key: None,
            foreign_keys: vec![],
        };
        for item in split_sql_items(body) {
            let item = item.trim();
            if let Some(captures) = foreign_key.captures(item) {
                table.foreign_keys.push(SqlForeignKey {
                    column: normalize_property_name(&captures[1]),
                    target_table: captures[2].to_string(),
                    target_column: normalize_property_name(&captures[3]),
                });
                continue;
            }
            if let Some(columns) = primary_key_columns(item) {
                table.primary_key = columns.into_iter().next();
                continue;
            }
            let mut words = item.split_whitespace();
            let Some(column) = words.next() else { continue };
            let Some(sql_type) = words.next() else {
                continue;
            };
            if matches!(
                column.to_ascii_uppercase().as_str(),
                "CONSTRAINT" | "PRIMARY" | "FOREIGN" | "UNIQUE" | "CHECK"
            ) {
                continue;
            }
            let column = normalize_property_name(column);
            let not_null = item.to_ascii_uppercase().contains("NOT NULL")
                || item.to_ascii_uppercase().contains("PRIMARY KEY");
            if item.to_ascii_uppercase().contains("PRIMARY KEY") {
                table.primary_key = Some(column.clone());
            }
            if let Some(captures) = inline_reference.captures(item) {
                table.foreign_keys.push(SqlForeignKey {
                    column: column.clone(),
                    target_table: captures[3].to_string(),
                    target_column: normalize_property_name(&captures[4]),
                });
            }
            table.columns.push(SqlColumn {
                name: column,
                sql_type: sql_type.to_string(),
                not_null,
            });
        }
        if table.columns.is_empty() {
            return Err(format!(
                "CREATE TABLE `{}` has no supported columns",
                table.name
            ));
        }
        tables.push(table);
    }
    Ok(tables)
}

fn balanced_sql_body(input: &str) -> Option<&str> {
    let mut depth = 1usize;
    for (index, character) in input.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&input[..index]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_sql_items(body: &str) -> Vec<&str> {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut items = Vec::new();
    for (index, character) in body.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    items.push(&body[start..]);
    items
}

fn primary_key_columns(item: &str) -> Option<Vec<String>> {
    let primary_key = Regex::new(r"(?i)(?:CONSTRAINT\s+\w+\s+)?PRIMARY\s+KEY\s*\(([^)]*)\)")
        .expect("constant primary key regex is valid");
    primary_key.captures(item).map(|captures| {
        captures[1]
            .split(',')
            .map(normalize_property_name)
            .filter(|name| !name.is_empty())
            .collect()
    })
}

fn sql_property_type(sql_type: &str) -> PropertyType {
    let sql_type = sql_type.to_ascii_uppercase();
    if sql_type.contains("INT") || sql_type == "SERIAL" || sql_type == "BIGSERIAL" {
        PropertyType::Integer
    } else if ["NUMERIC", "DECIMAL", "REAL", "FLOAT", "DOUBLE", "MONEY"]
        .iter()
        .any(|name| sql_type.contains(name))
    {
        PropertyType::Number
    } else if sql_type.contains("BOOL") {
        PropertyType::Boolean
    } else if sql_type.contains("TIMESTAMP") || sql_type == "DATETIME" {
        PropertyType::DateTime
    } else if sql_type == "DATE" || sql_type == "TIME" {
        PropertyType::DateTime
    } else {
        PropertyType::String
    }
}

fn property_from_schema(name: &str, definition: &Value, required: &[&str]) -> PropertySpec {
    let enum_values: Vec<String> = definition
        .get("enum")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let schema_type = definition.get("type").and_then(Value::as_str).or_else(|| {
        definition
            .get("type")
            .and_then(Value::as_array)
            .and_then(|types| types.iter().find_map(Value::as_str))
    });
    let prop_type = if !enum_values.is_empty() {
        PropertyType::Enum
    } else {
        match schema_type {
            Some("integer") => PropertyType::Integer,
            Some("number") => PropertyType::Number,
            Some("boolean") => PropertyType::Boolean,
            Some("string")
                if definition.get("format").and_then(Value::as_str) == Some("date-time") =>
            {
                PropertyType::DateTime
            }
            Some("string") => PropertyType::String,
            _ => PropertyType::Text,
        }
    };
    PropertySpec {
        name: normalize_property_name(name),
        label: definition
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_string(),
        prop_type,
        required: required.contains(&name),
        description: definition
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        enum_values,
    }
}

fn choose_primary_key(headers: &[String]) -> String {
    headers
        .iter()
        .find(|name| name.as_str() == "id" || name.ends_with("_id"))
        .cloned()
        .unwrap_or_else(|| headers[0].clone())
}

fn normalize_property_name(value: &str) -> String {
    value
        .trim()
        .chars()
        .fold((String::new(), false), |(mut out, separator), ch| {
            if ch.is_ascii_alphanumeric() {
                if separator && !out.is_empty() {
                    out.push('_');
                }
                out.push(ch.to_ascii_lowercase());
                (out, false)
            } else {
                (out, true)
            }
        })
        .0
}

fn normalize_type_id(value: &str) -> String {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn csv_headers_produce_a_safe_object_draft() {
        let draft = from_csv_headers(
            "vehicle_id,Display Name,active\nv1,Leaf,true\n",
            Some("vehicle record"),
            None,
            None,
            vec![],
        )
        .unwrap();
        let object = &draft.object_types[0];
        assert_eq!(object.id, "VehicleRecord");
        assert_eq!(object.primary_key, "vehicle_id");
        assert!(object
            .properties
            .iter()
            .all(|property| matches!(property.prop_type, PropertyType::String)));
        assert!(draft.link_types.is_empty());
    }

    #[test]
    fn json_schema_maps_types_required_and_enums() {
        let draft = from_json_schema(
            &json!({
                "title": "Device",
                "required": ["serial", "enabled"],
                "properties": {
                    "serial": {"type": "string"},
                    "enabled": {"type": "boolean"},
                    "count": {"type": "integer"},
                    "state": {"enum": ["new", "old"]},
                    "observed_at": {"type": "string", "format": "date-time"}
                }
            }),
            None,
            None,
            vec![],
        )
        .unwrap();
        let properties = &draft.object_types[0].properties;
        assert!(properties.iter().any(|property| property.name == "enabled"
            && property.required
            && matches!(property.prop_type, PropertyType::Boolean)));
        assert!(properties.iter().any(|property| property.name == "state"
            && matches!(property.prop_type, PropertyType::Enum)
            && property.enum_values == ["new", "old"]));
        assert!(properties
            .iter()
            .any(|property| property.name == "observed_at"
                && matches!(property.prop_type, PropertyType::DateTime)));
    }

    #[test]
    fn openapi_fixture_creates_types_and_only_explicit_annotation_link() {
        let document: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/ontology_draft/openapi_3.json"
        ))
        .unwrap();
        let draft = from_openapi(&document, vec![]).unwrap();

        assert_eq!(draft.object_types.len(), 2);
        assert!(draft.object_types.iter().any(|object| {
            object.id == "Asset"
                && object.properties.iter().any(|property| {
                    property.name == "active"
                        && property.required
                        && matches!(property.prop_type, PropertyType::Boolean)
                })
        }));
        assert_eq!(draft.link_types.len(), 1);
        assert_eq!(draft.link_types[0].id, "AssetAtSite");
        assert!(draft.suggested_links.is_empty());
        assert!(draft
            .warnings
            .iter()
            .any(|warning| warning.contains("not inferred")));
    }

    #[test]
    fn sql_ddl_fixture_creates_types_and_foreign_key_link_only() {
        let draft = from_sql_ddl(
            include_str!("../../tests/fixtures/ontology_draft/inventory.sql"),
            vec![],
        )
        .unwrap();

        let asset = draft
            .object_types
            .iter()
            .find(|object| object.id == "Assets")
            .unwrap();
        assert_eq!(asset.primary_key, "asset_id");
        assert!(asset.properties.iter().any(|property| {
            property.name == "capacity_kw" && matches!(property.prop_type, PropertyType::Number)
        }));
        assert!(asset.properties.iter().any(|property| {
            property.name == "active"
                && property.required
                && matches!(property.prop_type, PropertyType::Boolean)
        }));
        assert_eq!(draft.link_types.len(), 1);
        assert_eq!(draft.link_types[0].source, "Assets");
        assert_eq!(draft.link_types[0].target, "Sites");
        assert!(matches!(
            draft.link_types[0].cardinality,
            Cardinality::ManyToOne
        ));
    }

    #[test]
    fn openapi_rejects_non_v3_documents() {
        let error = from_openapi(
            &json!({"openapi": "2.0", "components": {"schemas": {}}}),
            vec![],
        )
        .unwrap_err();
        assert!(error.contains("OpenAPI 3.x"));
    }
}
