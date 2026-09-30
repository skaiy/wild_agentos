#![allow(deprecated)]

use std::sync::Arc;

use serde_json::json;
use tracing::{debug, info, warn};

use crate::batch::error::BatchError;
use crate::batch::types::{
    BatchAgentConfig, ExtractedEntity, ExtractedRelation, ExtractionResult, PersistReport,
};
use crate::knowledge_graph::store::KnowledgeGraphStore;
use crate::knowledge_graph::types::{RdfQuad, RdfValue};
use crate::memory::l0_store::L0Store;
use crate::memory::memory_manager::MemoryManager;

pub struct KnowledgePersister {
    kg_store: Option<Arc<KnowledgeGraphStore>>,
    #[allow(dead_code)]
    memory_manager: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
    l0_store: Option<Arc<L0Store>>,
}

impl KnowledgePersister {
    pub fn new(
        kg_store: Option<Arc<KnowledgeGraphStore>>,
        memory_manager: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
        l0_store: Option<Arc<L0Store>>,
    ) -> Self {
        Self {
            kg_store,
            memory_manager,
            l0_store,
        }
    }

    pub async fn persist(
        &self,
        result: &ExtractionResult,
        config: &BatchAgentConfig,
    ) -> Result<PersistReport, BatchError> {
        let graph = self.resolve_graph(config);
        let mut report = PersistReport {
            entities_persisted: 0,
            relations_persisted: 0,
            new_entities: 0,
            updated_entities: 0,
            named_graph: graph.clone(),
            task_iri: Some(format!("batch://{}", config.name)),
        };

        // Persist entities
        if !result.entities.is_empty() {
            match self.persist_entities(&result.entities, &config.business_domain, &graph) {
                Ok(iris) => {
                    report.entities_persisted = result.entities.len();
                    report.new_entities = iris.len();
                    debug!(
                        agent = %config.name,
                        entities = %result.entities.len(),
                        "Entities persisted"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "Failed to persist entities");
                }
            }
        }

        // Persist relations
        if !result.relations.is_empty() {
            match self.persist_relations(&result.relations, &config.business_domain, &graph) {
                Ok(count) => {
                    report.relations_persisted = count;
                    debug!(
                        agent = %config.name,
                        relations = %count,
                        "Relations persisted"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "Failed to persist relations");
                }
            }
        }

        // Persist to memory
        if let Err(e) = self.persist_to_memory(result, &report.task_iri.clone().unwrap_or_default())
        {
            warn!(error = %e, "Failed to persist to memory");
        }

        info!(
            agent = %config.name,
            graph = %graph,
            entities = %report.entities_persisted,
            relations = %report.relations_persisted,
            "Knowledge persist completed"
        );

        Ok(report)
    }

    pub fn persist_entities(
        &self,
        entities: &[ExtractedEntity],
        domain: &str,
        graph: &str,
    ) -> Result<Vec<String>, BatchError> {
        let store = match self.kg_store.as_ref() {
            Some(s) => s,
            None => return Ok(vec![]),
        };

        let mut iris = Vec::new();
        let mut quads = Vec::new();

        for entity in entities {
            let entity_iri = format!(
                "iri://entity/batch/{}/{}",
                sanitize_id(&entity.entity_type),
                sanitize_id(&entity.name)
            );

            // rdf:type assertion
            quads.push(RdfQuad {
                subject: entity_iri.clone(),
                predicate: "http://www.w3.org/1999/02/22-rdf-syntax-ns#type".into(),
                object: RdfValue::Iri(format!(
                    "https://agentos.ontology/batch/{}",
                    entity.entity_type
                )),
                graph: Some(graph.into()),
            });

            // rdfs:label
            quads.push(RdfQuad {
                subject: entity_iri.clone(),
                predicate: "http://www.w3.org/2000/01/rdf-schema#label".into(),
                object: RdfValue::Literal(entity.name.clone()),
                graph: Some(graph.into()),
            });

            // confidence
            quads.push(RdfQuad {
                subject: entity_iri.clone(),
                predicate: "https://agentos.ontology/core/confidence".into(),
                object: RdfValue::TypedLiteral(
                    entity.confidence.to_string(),
                    "http://www.w3.org/2001/XMLSchema#float".into(),
                ),
                graph: Some(graph.into()),
            });

            // description (if present)
            if let Some(ref desc) = entity.description {
                quads.push(RdfQuad {
                    subject: entity_iri.clone(),
                    predicate: "http://www.w3.org/2000/01/rdf-schema#comment".into(),
                    object: RdfValue::Literal(desc.clone()),
                    graph: Some(graph.into()),
                });
            }

            // Source batch
            quads.push(RdfQuad {
                subject: entity_iri.clone(),
                predicate: "https://agentos.ontology/core/source".into(),
                object: RdfValue::Iri(format!("batch://source/{}", domain)),
                graph: Some(graph.into()),
            });

            iris.push(entity_iri);
        }

        store
            .write_quads(&quads, graph)
            .map_err(|e| BatchError::KgWriteFailed { message: e })?;

        Ok(iris)
    }

    pub fn persist_relations(
        &self,
        relations: &[ExtractedRelation],
        domain: &str,
        graph: &str,
    ) -> Result<usize, BatchError> {
        let store = match self.kg_store.as_ref() {
            Some(s) => s,
            None => return Ok(0),
        };

        let mut quads = Vec::new();

        for relation in relations {
            let from_iri = format!(
                "iri://entity/batch/{}/{}",
                domain,
                sanitize_id(&relation.from)
            );
            let to_iri = format!(
                "iri://entity/batch/{}/{}",
                domain,
                sanitize_id(&relation.to)
            );
            let rel_iri = format!("https://agentos.ontology/relation/{}", relation.relation);

            quads.push(RdfQuad {
                subject: from_iri,
                predicate: rel_iri,
                object: RdfValue::Iri(to_iri),
                graph: Some(graph.into()),
            });
        }

        store
            .write_quads(&quads, graph)
            .map_err(|e| BatchError::KgWriteFailed { message: e })?;

        Ok(relations.len())
    }

    pub fn persist_to_memory(
        &self,
        result: &ExtractionResult,
        task_iri: &str,
    ) -> Result<(), BatchError> {
        // Store extraction summary in L0 for later retrieval
        if let Some(ref l0) = self.l0_store {
            let summary = json!({
                "type": "batch_extraction",
                "task_iri": task_iri,
                "extracted_at": result.extracted_at.to_rfc3339(),
                "batch_id": result.batch_id,
                "entities_count": result.entities.len(),
                "relations_count": result.relations.len(),
                "context_summary": result.context_summary,
                "intent": result.intent.as_ref().map(|i| json!({
                    "type": i.intent_type,
                    "confidence": i.confidence,
                })),
            })
            .to_string();

            let _ = l0.store(task_iri, &summary);
        }

        Ok(())
    }

    fn resolve_graph(&self, config: &BatchAgentConfig) -> String {
        format!("graph:batch/{}/{}", config.name, config.business_domain)
    }
}

fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | ':' => c,
            ' ' | '\t' | '\n' => '_',
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("Hello World"), "Hello_World");
        assert_eq!(sanitize_id("test-id_123"), "test-id_123");
        assert_eq!(sanitize_id("special@#$%chars"), "special____chars");
    }

    #[test]
    fn test_resolve_graph() {
        let config = BatchAgentConfig {
            name: "test_agent".to_string(),
            business_domain: "test_domain".to_string(),
            ..Default::default()
        };

        let persister = KnowledgePersister::new(None, None, None);
        let graph = persister.resolve_graph(&config);
        assert_eq!(graph, "graph:batch/test_agent/test_domain");
    }
}
