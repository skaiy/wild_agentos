use std::sync::Arc;

use super::store::KnowledgeGraphStore;
use super::types::{BridgeRelationType, RdfQuad, RdfValue};

/// Entity-to-skill relation bridge backed by Oxigraph.
///
/// **Note**: This struct is only instantiated in unit tests. Production code
/// uses `UnifiedGraphStore::with_shared_store()` + `SkillGraphStore::with_oxi_store()`
/// for cross-subsystem Oxigraph sharing. See `src/memory/unified_graph.rs`.
pub struct KnowledgeBridge {
    store: KnowledgeGraphStore,
    bridge_graph: String,
}

impl KnowledgeBridge {
    pub fn new() -> Result<Self, String> {
        let store = KnowledgeGraphStore::new().map_err(|error| error.to_string())?;
        Ok(Self {
            store,
            bridge_graph: "graph:bridge".to_string(),
        })
    }

    pub fn with_shared_store(store: Arc<oxigraph::store::Store>) -> Result<Self, String> {
        let store = KnowledgeGraphStore::with_shared_store(store)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            store,
            bridge_graph: "graph:bridge".to_string(),
        })
    }

    /// Bridge backed by an OO `SharedGraphStore` (feature-gated).
    #[cfg(feature = "ontology")]
    pub fn with_shared_graph_store(
        shared: &crate::ontology::SharedGraphStore,
    ) -> Result<Self, String> {
        let store = KnowledgeGraphStore::with_shared_graph_store(shared)?;
        Ok(Self {
            store,
            bridge_graph: "graph:bridge".to_string(),
        })
    }

    fn relation_to_iri(relation: &BridgeRelationType) -> &'static str {
        match relation {
            BridgeRelationType::HasSkill => "https://agentos.ontology/bridge/hasSkill",
            BridgeRelationType::ApplicableIn => "https://agentos.ontology/bridge/applicableIn",
            BridgeRelationType::RelatedTo => "https://agentos.ontology/bridge/relatedTo",
        }
    }

    pub fn create_bridge(
        &self,
        entity_id: &str,
        skill_iri: &str,
        relation: BridgeRelationType,
    ) -> Result<(), String> {
        let entity_iri = format!("iri://entity/{}", entity_id);
        let predicate = Self::relation_to_iri(&relation);

        let quad = RdfQuad {
            subject: entity_iri,
            predicate: predicate.to_string(),
            object: RdfValue::Iri(skill_iri.to_string()),
            graph: Some(self.bridge_graph.clone()),
        };

        self.store.write_quads(&[quad], &self.bridge_graph)
    }

    pub fn query_bridged_skills(&self, entity_id: &str) -> Result<Vec<String>, String> {
        let entity_iri = format!("iri://entity/{}", entity_id);
        let sparql = format!(
            "SELECT ?relation ?skill WHERE {{ <{}> ?relation ?skill }}",
            entity_iri
        );

        let results = self.store.query_sparql(&sparql, Some(&self.bridge_graph))?;

        let skills = results
            .iter()
            .filter_map(|row| row.get("?skill").and_then(|v| v.as_str()).map(String::from))
            .collect();

        Ok(skills)
    }

    pub fn query_bridged_entities(&self, skill_iri: &str) -> Result<Vec<String>, String> {
        let sparql = format!(
            "SELECT ?entity ?relation WHERE {{ ?entity ?relation <{}> }}",
            skill_iri
        );

        let results = self.store.query_sparql(&sparql, Some(&self.bridge_graph))?;

        let entities = results
            .iter()
            .filter_map(|row| {
                row.get("?entity")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect();

        Ok(entities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_query_bridge() {
        let bridge = KnowledgeBridge::new().unwrap();

        bridge
            .create_bridge(
                "task_001",
                "iri://skill/code_review",
                BridgeRelationType::HasSkill,
            )
            .unwrap();

        bridge
            .create_bridge(
                "task_001",
                "iri://skill/testing",
                BridgeRelationType::RelatedTo,
            )
            .unwrap();

        let skills = bridge.query_bridged_skills("task_001").unwrap();
        assert_eq!(skills.len(), 2, "should query 2 associated skills");
        assert!(
            skills.contains(&"iri://skill/code_review".to_string()),
            "should include code_review"
        );
        assert!(
            skills.contains(&"iri://skill/testing".to_string()),
            "should include testing"
        );
    }

    #[test]
    fn test_bridge_skills() {
        let bridge = KnowledgeBridge::new().unwrap();

        bridge
            .create_bridge(
                "entity_alpha",
                "iri://skill/analysis",
                BridgeRelationType::ApplicableIn,
            )
            .unwrap();
        bridge
            .create_bridge(
                "entity_alpha",
                "iri://skill/design",
                BridgeRelationType::HasSkill,
            )
            .unwrap();

        let skills = bridge.query_bridged_skills("entity_alpha").unwrap();
        assert_eq!(skills.len(), 2, "should query 2 skills");

        let empty = bridge.query_bridged_skills("nonexistent").unwrap();
        assert!(
            empty.is_empty(),
            "nonexistent entity should return empty list"
        );
    }

    #[test]
    fn test_bridge_entities() {
        let bridge = KnowledgeBridge::new().unwrap();

        bridge
            .create_bridge(
                "entity_x",
                "iri://skill/debug",
                BridgeRelationType::HasSkill,
            )
            .unwrap();
        bridge
            .create_bridge(
                "entity_y",
                "iri://skill/debug",
                BridgeRelationType::RelatedTo,
            )
            .unwrap();

        let entities = bridge.query_bridged_entities("iri://skill/debug").unwrap();
        assert_eq!(entities.len(), 2, "should query 2 associated entities");
        assert!(
            entities.contains(&"iri://entity/entity_x".to_string()),
            "should include entity_x"
        );
        assert!(
            entities.contains(&"iri://entity/entity_y".to_string()),
            "should include entity_y"
        );

        let empty = bridge
            .query_bridged_entities("iri://skill/nonexistent")
            .unwrap();
        assert!(
            empty.is_empty(),
            "nonexistent skill should return empty list"
        );
    }
}
