use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use oxigraph::model::{Literal, NamedNode, Quad, Term};
use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// A knowledge graph entity with temporal metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub types: Vec<String>,
    pub properties: HashMap<String, PropertyValue>,
    /// When this entity was created
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    /// When this entity was last modified
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PropertyValue {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Reference(String),
    Array(Vec<PropertyValue>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    pub subject: String,
    pub predicate: String,
    pub object: RelationObject,
    /// When this relation was created
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelationObject {
    Node(String),
    Value(PropertyValue),
}

#[derive(Debug, Clone)]
pub struct SparqlQueryResult {
    pub variables: Vec<String>,
    pub bindings: Vec<HashMap<String, SparqlValue>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SparqlValue {
    Uri(String),
    Literal(String, Option<String>),
    BlankNode(String),
}

#[derive(Debug, Clone)]
pub struct GraphStats {
    pub total_triples: usize,
    pub named_graphs: usize,
    pub entities: usize,
}

pub struct TransactionLog {
    operations: Vec<TransactionOperation>,
}

#[derive(Debug, Clone)]
pub enum TransactionOperation {
    InsertTriple {
        subject: String,
        predicate: String,
        object: String,
        graph: Option<String>,
    },
    DeleteTriple {
        subject: String,
        predicate: String,
        object: String,
        graph: Option<String>,
    },
}

impl Default for TransactionLog {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionLog {
    pub fn new() -> Self {
        Self {
            operations: Vec::new(),
        }
    }

    pub fn log_insert(
        &mut self,
        subject: &str,
        predicate: &str,
        object: &str,
        graph: Option<&str>,
    ) {
        self.operations.push(TransactionOperation::InsertTriple {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            graph: graph.map(|g| g.to_string()),
        });
    }

    pub fn log_delete(
        &mut self,
        subject: &str,
        predicate: &str,
        object: &str,
        graph: Option<&str>,
    ) {
        self.operations.push(TransactionOperation::DeleteTriple {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            graph: graph.map(|g| g.to_string()),
        });
    }

    pub fn clear(&mut self) {
        self.operations.clear();
    }

    pub fn len(&self) -> usize {
        self.operations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
}

pub struct UnifiedGraphStore {
    store: Arc<Store>,
    default_graph: String,
    transaction_log: RwLock<TransactionLog>,
    in_transaction: RwLock<bool>,
}

impl UnifiedGraphStore {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        info!("Initializing Unified Oxigraph Store (memory)");
        Ok(Self {
            store: Arc::new(Store::new()?),
            default_graph: "https://wildagentos.org/graph/default".to_string(),
            transaction_log: RwLock::new(TransactionLog::new()),
            in_transaction: RwLock::new(false),
        })
    }

    pub fn new_persistent<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        info!(path = %path.display(), "Initializing Unified Oxigraph Store (persistent, RocksDB)");
        Ok(Self {
            store: Arc::new(Store::open(path)?),
            default_graph: "https://wildagentos.org/graph/default".to_string(),
            transaction_log: RwLock::new(TransactionLog::new()),
            in_transaction: RwLock::new(false),
        })
    }

    /// Create a UnifiedGraphStore backed by a shared Oxigraph `Arc<Store>`.
    ///
    /// All subsystems (skill graph, knowledge bridge, blackboard) that receive
    /// the same `Arc<Store>` will share the underlying Oxigraph store, enabling
    /// cross-subsystem SPARQL joins via named graphs.
    ///
    /// This is the production pattern used by the gRPC server — it creates one
    /// `UnifiedGraphStore` and passes `Arc::clone(&store.store())` to all consumers
    /// via their respective `with_shared_store()` / `with_oxi_store()` builders.
    pub fn with_shared_store(store: Arc<Store>) -> Self {
        Self {
            store,
            default_graph: "https://wildagentos.org/graph/default".to_string(),
            transaction_log: RwLock::new(TransactionLog::new()),
            in_transaction: RwLock::new(false),
        }
    }

    pub fn store(&self) -> Arc<Store> {
        self.store.clone()
    }

    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.store)
    }

    fn parse_uri(&self, uri: &str) -> NamedNode {
        NamedNode::new_unchecked(uri)
    }
    fn term_to_string(&self, term: &Term) -> String {
        match term {
            Term::NamedNode(node) => node.as_str().to_string(),
            Term::BlankNode(node) => node.as_str().to_string(),
            Term::Literal(lit) => {
                let value = lit.value();
                if let Some(lang) = lit.language() {
                    format!("\"{}\"@{}", value, lang)
                } else {
                    let datatype = lit.datatype();
                    format!("\"{}\"^^<{}>", value, datatype.as_str())
                }
            }
            Term::Triple(triple) => format!(
                "<< {} {} {} >>",
                triple.subject, triple.predicate, triple.object
            ),
        }
    }

    pub fn add_entity(
        &self,
        entity: &Entity,
        graph: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let graph_uri = graph.unwrap_or(&self.default_graph);
        let graph_node = self.parse_uri(graph_uri);
        let subject = self.parse_uri(&entity.id);

        for type_uri in &entity.types {
            let quad = Quad::new(
                subject.clone(),
                NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                self.parse_uri(type_uri),
                graph_node.clone(),
            );
            self.store.insert(&quad)?;
            self.transaction_log.write().log_insert(
                &entity.id,
                "rdf:type",
                type_uri,
                Some(graph_uri),
            );
        }

        for (predicate, value) in &entity.properties {
            let predicate_node = NamedNode::new_unchecked(predicate);
            let object_term = self.property_value_to_term(value);
            let quad = Quad::new(
                subject.clone(),
                predicate_node,
                object_term.clone(),
                graph_node.clone(),
            );
            self.store.insert(&quad)?;
            self.transaction_log.write().log_insert(
                &entity.id,
                predicate,
                &self.term_to_string(&object_term),
                Some(graph_uri),
            );
        }

        debug!(entity_id = %entity.id, graph = %graph_uri, "Entity added");
        Ok(())
    }

    fn property_value_to_term(&self, value: &PropertyValue) -> Term {
        match value {
            PropertyValue::String(s) => Term::Literal(Literal::new_simple_literal(s)),
            PropertyValue::Integer(n) => Term::Literal(Literal::new_typed_literal(
                n.to_string(),
                NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer"),
            )),
            PropertyValue::Float(f) => Term::Literal(Literal::new_typed_literal(
                f.to_string(),
                NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#double"),
            )),
            PropertyValue::Boolean(b) => Term::Literal(Literal::new_typed_literal(
                b.to_string(),
                NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#boolean"),
            )),
            PropertyValue::Reference(uri) => Term::NamedNode(self.parse_uri(uri)),
            PropertyValue::Array(items) => {
                if items.is_empty() {
                    Term::Literal(Literal::new_simple_literal(""))
                } else {
                    self.property_value_to_term(&items[0])
                }
            }
        }
    }

    pub fn get_entity(&self, id: &str, graph: Option<&str>) -> Option<Entity> {
        let subject = self.parse_uri(id);
        let mut types = Vec::new();
        let mut properties = HashMap::new();

        let rdf_type = NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");

        let graph_node = graph.map(|g| self.parse_uri(g));
        let graph_name: Option<oxigraph::model::GraphNameRef<'_>> =
            graph_node.as_ref().map(|node| node.as_ref().into());

        let results: Vec<Quad> = self
            .store
            .quads_for_pattern(Some(subject.as_ref().into()), None, None, graph_name)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;

        for quad in &results {
            if quad.predicate == rdf_type {
                if let Term::NamedNode(node) = &quad.object {
                    types.push(node.as_str().to_string());
                }
            } else {
                let value = self.term_to_property_value(&quad.object);
                properties.insert(quad.predicate.as_str().to_string(), value);
            }
        }

        if types.is_empty() && properties.is_empty() {
            None
        } else {
            let now = Utc::now();
            Some(Entity {
                id: id.to_string(),
                types,
                properties,
                created_at: now,
                updated_at: now,
            })
        }
    }

    fn term_to_property_value(&self, term: &Term) -> PropertyValue {
        match term {
            Term::NamedNode(node) => PropertyValue::Reference(node.as_str().to_string()),
            Term::Literal(lit) => {
                let value = lit.value();
                if lit.language().is_some() {
                    return PropertyValue::String(value.to_string());
                }
                let dtype = lit.datatype().as_str();
                if dtype.contains("integer") {
                    value
                        .parse::<i64>()
                        .map(PropertyValue::Integer)
                        .unwrap_or_else(|_| PropertyValue::String(value.to_string()))
                } else if dtype.contains("double")
                    || dtype.contains("float")
                    || dtype.contains("decimal")
                {
                    value
                        .parse::<f64>()
                        .map(PropertyValue::Float)
                        .unwrap_or_else(|_| PropertyValue::String(value.to_string()))
                } else if dtype.contains("boolean") {
                    value
                        .parse::<bool>()
                        .map(PropertyValue::Boolean)
                        .unwrap_or_else(|_| PropertyValue::String(value.to_string()))
                } else {
                    PropertyValue::String(value.to_string())
                }
            }
            _ => PropertyValue::String(term.to_string()),
        }
    }

    pub fn update_entity(
        &self,
        entity: &Entity,
        graph: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.delete_entity(&entity.id, graph)?;
        self.add_entity(entity, graph)?;
        debug!(entity_id = %entity.id, "Entity updated");
        Ok(())
    }

    pub fn delete_entity(
        &self,
        id: &str,
        graph: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let subject = self.parse_uri(id);
        let graph_uri = graph.unwrap_or(&self.default_graph);
        let graph_ref = self.parse_uri(graph_uri);

        let quads_to_remove: Vec<Quad> = self
            .store
            .quads_for_pattern(
                Some(subject.as_ref().into()),
                None,
                None,
                Some(graph_ref.as_ref().into()),
            )
            .collect::<Result<Vec<_>, _>>()?;

        for quad in &quads_to_remove {
            self.store.remove(quad)?;
        }

        debug!(entity_id = %id, removed = quads_to_remove.len(), "Entity deleted");
        Ok(())
    }

    pub fn add_relation(
        &self,
        relation: &Relation,
        graph: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let graph_uri = graph.unwrap_or(&self.default_graph);
        let graph_node = self.parse_uri(graph_uri);
        let subject = self.parse_uri(&relation.subject);
        let predicate = NamedNode::new_unchecked(&relation.predicate);
        let object = match &relation.object {
            RelationObject::Node(uri) => self.parse_uri(uri).into(),
            RelationObject::Value(v) => self.property_value_to_term(v),
        };

        let quad = Quad::new(subject, predicate, object, graph_node);
        self.store.insert(&quad)?;

        debug!(subject = %relation.subject, predicate = %relation.predicate, "Relation added");
        Ok(())
    }

    pub fn delete_relation(
        &self,
        relation: &Relation,
        graph: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let graph_uri = graph.unwrap_or(&self.default_graph);
        let graph_node = self.parse_uri(graph_uri);
        let subject = self.parse_uri(&relation.subject);
        let predicate = NamedNode::new_unchecked(&relation.predicate);
        let object = match &relation.object {
            RelationObject::Node(uri) => self.parse_uri(uri).into(),
            RelationObject::Value(v) => self.property_value_to_term(v),
        };

        let quad = Quad::new(subject, predicate, object, graph_node);
        self.store.remove(&quad)?;

        debug!(subject = %relation.subject, predicate = %relation.predicate, "Relation deleted");
        Ok(())
    }

    #[allow(deprecated)]
    pub fn query(&self, sparql: &str) -> Result<SparqlQueryResult, Box<dyn std::error::Error>> {
        debug!(sparql_len = sparql.len(), "Executing SPARQL query");

        let results = self.store.query(sparql)?;
        let mut variables = Vec::new();
        let mut bindings = Vec::new();

        match results {
            QueryResults::Solutions(solutions) => {
                for result in solutions {
                    let result = result?;
                    if variables.is_empty() {
                        variables = result
                            .variables()
                            .iter()
                            .map(|v| v.as_str().to_string())
                            .collect();
                    }

                    let mut row = HashMap::new();
                    for (var, value) in result.iter() {
                        let sparql_value = match value {
                            Term::NamedNode(node) => SparqlValue::Uri(node.as_str().to_string()),
                            Term::Literal(lit) => {
                                let lang = lit.language().map(|l| l.to_string());
                                SparqlValue::Literal(lit.value().to_string(), lang)
                            }
                            Term::BlankNode(node) => {
                                SparqlValue::BlankNode(node.as_str().to_string())
                            }
                            Term::Triple(triple) => SparqlValue::Literal(
                                format!(
                                    "<< {} {} {} >>",
                                    triple.subject, triple.predicate, triple.object
                                ),
                                None,
                            ),
                        };
                        row.insert(var.as_str().to_string(), sparql_value);
                    }
                    bindings.push(row);
                }
            }
            QueryResults::Boolean(b) => {
                debug!(result = b, "SPARQL ASK query completed");
            }
            QueryResults::Graph(_graph) => {
                debug!("SPARQL CONSTRUCT/DESCRIBE query completed");
            }
        }

        debug!(
            variables = variables.len(),
            bindings = bindings.len(),
            "SPARQL query completed"
        );
        Ok(SparqlQueryResult {
            variables,
            bindings,
        })
    }

    pub fn query_as_json(&self, sparql: &str) -> Result<String, Box<dyn std::error::Error>> {
        let result = self.query(sparql)?;
        let json = serde_json::to_string(&serde_json::json!({
            "variables": result.variables,
            "bindings": result.bindings.iter().map(|row| {
                let mut map = serde_json::Map::new();
                for (k, v) in row {
                    let value = match v {
                        SparqlValue::Uri(uri) => serde_json::json!({"type": "uri", "value": uri}),
                        SparqlValue::Literal(val, Some(lang)) => serde_json::json!({"type": "literal", "value": val, "lang": lang}),
                        SparqlValue::Literal(val, None) => serde_json::json!({"type": "literal", "value": val}),
                        SparqlValue::BlankNode(id) => serde_json::json!({"type": "bnode", "value": id}),
                    };
                    map.insert(k.clone(), value);
                }
                serde_json::Value::Object(map)
            }).collect::<Vec<_>>()
        }))?;
        Ok(json)
    }

    pub fn update(&self, sparql: &str) -> Result<(), Box<dyn std::error::Error>> {
        debug!(sparql_len = sparql.len(), "Executing SPARQL update");
        self.store.update(sparql)?;
        debug!("SPARQL update completed");
        Ok(())
    }

    pub fn create_named_graph(&self, graph_uri: &str) -> Result<(), Box<dyn std::error::Error>> {
        let graph_node = self.parse_uri(graph_uri);
        let quad = Quad::new(
            graph_node.clone(),
            NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
            NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#NamedGraph"),
            graph_node,
        );
        self.store.insert(&quad)?;
        info!(graph = %graph_uri, "Named graph created");
        Ok(())
    }

    pub fn drop_named_graph(&self, graph_uri: &str) -> Result<(), Box<dyn std::error::Error>> {
        let graph_node = self.parse_uri(graph_uri);
        let quads_to_remove: Vec<Quad> = self
            .store
            .quads_for_pattern(None, None, None, Some(graph_node.as_ref().into()))
            .collect::<Result<Vec<_>, _>>()?;

        for quad in &quads_to_remove {
            self.store.remove(quad)?;
        }

        info!(graph = %graph_uri, removed = quads_to_remove.len(), "Named graph dropped");
        Ok(())
    }

    pub fn list_named_graphs(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let graphs: Vec<String> = self
            .store
            .named_graphs()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|g| g.to_string())
            .collect();
        Ok(graphs)
    }

    pub fn begin_transaction(&self) {
        *self.in_transaction.write() = true;
        self.transaction_log.write().clear();
        debug!("Transaction begun");
    }

    pub fn commit_transaction(&self) -> Result<(), Box<dyn std::error::Error>> {
        *self.in_transaction.write() = false;
        let ops = self.transaction_log.read().len();
        self.transaction_log.write().clear();
        debug!(operations = ops, "Transaction committed");
        Ok(())
    }

    pub fn rollback_transaction(&self) {
        *self.in_transaction.write() = false;
        let ops = self.transaction_log.read().len();
        self.transaction_log.write().clear();
        debug!(operations = ops, "Transaction rolled back");
    }
}
