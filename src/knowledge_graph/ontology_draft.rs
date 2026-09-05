//! Conservative adapters that turn tabular/schema inputs into reviewable
//! ontology type drafts. They deliberately never create ActionTypes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ontology_layer::{
    ev, Cardinality, LinkType, ObjectKind, ObjectType, PropertySpec, PropertyType,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeDraftBundle {
    pub object_types: Vec<ObjectType>,
    pub link_types: Vec<LinkType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
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
        warnings: vec!["No ActionType is generated from JSON Schema input.".into()],
    })
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
}
