use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::isolation::IsolationClaims;

use super::rdf_mapper::RdfMapper;
use super::types::RdfQuad;

pub struct KnowledgeGraphStore {
    store: Arc<Store>,
    default_graph: String,
}

/// A mutation whose target graph is minted by [`KnowledgeGraphStore`] from
/// verified claims. The payload is graph-local SPARQL only.
#[derive(Debug, Clone)]
pub enum ClaimsGraphUpdate {
    InsertData(String),
    DeleteWhere(String),
}

/// A human approval that owns one claims-scoped staging graph.
///
/// Approval records live in a graph separate from the production data graph,
/// so queuing an action never makes its domain triples queryable as production
/// data. The staging graph remains claims-derived and cannot be selected by a
/// caller.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PendingActionApproval {
    pub approval_id: String,
    pub staging_id: String,
    pub staging_graph: String,
    pub action_id: String,
    pub created_at: String,
    pub expires_at: String,
}

const APPROVAL_BASE_IRI: &str = "https://agentos.ontology/action-approval/";
const APPROVAL_VOCAB_IRI: &str = "https://agentos.ontology/action-approval/";

impl ClaimsGraphUpdate {
    pub fn insert_data(triples: impl Into<String>) -> Self {
        Self::InsertData(triples.into())
    }

    pub fn delete_where(pattern: impl Into<String>) -> Self {
        Self::DeleteWhere(pattern.into())
    }

    /// The graph-local mutation, useful for dry-run responses.
    pub fn sparql(&self) -> String {
        match self {
            Self::InsertData(triples) => format!("INSERT DATA {{ {} }}", triples),
            Self::DeleteWhere(pattern) => format!("DELETE WHERE {{ {} }}", pattern),
        }
    }
}

impl KnowledgeGraphStore {
    /// Create KG Store using a unified shared Oxigraph Store
    pub fn with_shared_store(store: Arc<Store>) -> Result<Self, String> {
        Ok(Self {
            store,
            default_graph: "graph:world".to_string(),
        })
    }

    pub fn new() -> Result<Self, String> {
        let store = Store::new().map_err(|e| format!("failed to create Oxigraph Store: {}", e))?;
        Ok(Self {
            store: Arc::new(store),
            default_graph: "graph:world".to_string(),
        })
    }

    /// Expose the inner `Arc<Store>` for GH ↔ OO store sharing.
    pub fn store_arc(&self) -> &Arc<Store> {
        &self.store
    }

    /// Create from an OO `SharedGraphStore` (integration point).
    #[cfg(feature = "ontology")]
    pub fn with_shared_graph_store(
        shared: &crate::ontology::SharedGraphStore,
    ) -> Result<Self, String> {
        Ok(Self {
            store: Arc::clone(shared.inner()),
            default_graph: "graph:world".to_string(),
        })
    }

    pub fn with_graph(graph_name: &str) -> Result<Self, String> {
        let store = Store::new().map_err(|e| format!("failed to create Oxigraph Store: {}", e))?;
        Ok(Self {
            store: Arc::new(store),
            default_graph: graph_name.to_string(),
        })
    }

    /// Writes are only permitted through [`Self::write_quads_for_claims`].
    ///
    /// Keeping this method as a failing compatibility shim prevents a caller
    /// from silently continuing to write new data into a caller-supplied graph,
    /// including the historical `graph:world` graph.
    #[deprecated(note = "new graph writes require write_quads_for_claims with verified claims")]
    pub fn write_quads(&self, quads: &[RdfQuad], graph: &str) -> Result<(), String> {
        #[cfg(test)]
        {
            if quads.is_empty() {
                return Ok(());
            }
            let sparql = RdfMapper::quads_to_sparql_insert(quads, graph);
            return self
                .store
                .update(&sparql)
                .map_err(|e| format!("SPARQL INSERT failed: {}", e));
        }
        #[cfg(not(test))]
        let _ = (quads, graph);
        Err("verified isolation claims are required for graph writes".to_string())
    }

    /// Writes quads to the graph minted from verified tenant and project claims.
    ///
    /// The quad graph fields and any caller-selected graph are deliberately
    /// ignored: new data can only enter `graph://{tenant}/{project}`.
    pub fn write_quads_for_claims(
        &self,
        claims: &IsolationClaims,
        quads: &[RdfQuad],
    ) -> Result<(), String> {
        let graph = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {}", e))?;
        if quads.is_empty() {
            return Ok(());
        }
        let sparql = RdfMapper::quads_to_sparql_insert(quads, &graph);
        self.store
            .update(&sparql)
            .map_err(|e| format!("SPARQL INSERT failed: {}", e))
    }

    /// Upserts KB catalog metadata in the graph minted from verified claims.
    ///
    /// The catalog entry subject is derived from the server-generated KB id.
    /// Neither the caller nor the entry controls the target graph.
    pub fn upsert_kb_catalog_metadata_for_claims(
        &self,
        claims: &IsolationClaims,
        kb_id: &str,
        name: &str,
        kb_type: &str,
    ) -> Result<(), String> {
        let graph = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {}", e))?;
        let subject = Self::kb_catalog_subject(kb_id);
        let delete = format!(
            "DELETE WHERE {{ GRAPH <{graph}> {{ <{subject}> <https://agentos.ontology/meta/kbName> ?o }} }}"
        );
        self.store
            .update(&delete)
            .map_err(|e| format!("KB catalog metadata delete failed: {}", e))?;
        let quads = [
            RdfQuad {
                subject: subject.clone(),
                predicate: "https://agentos.ontology/meta/kbName".to_string(),
                object: super::types::RdfValue::Literal(name.to_string()),
                graph: None,
            },
            RdfQuad {
                subject,
                predicate: "https://agentos.ontology/meta/kbType".to_string(),
                object: super::types::RdfValue::Literal(kb_type.to_string()),
                graph: None,
            },
        ];
        self.write_quads_for_claims(claims, &quads)
    }

    /// Deletes one server-generated KB catalog entry from the claims graph.
    pub fn delete_kb_catalog_metadata_for_claims(
        &self,
        claims: &IsolationClaims,
        kb_id: &str,
    ) -> Result<(), String> {
        let graph = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {}", e))?;
        let subject = Self::kb_catalog_subject(kb_id);
        let delete = format!("DELETE WHERE {{ GRAPH <{graph}> {{ <{subject}> ?p ?o }} }}");
        self.store
            .update(&delete)
            .map_err(|e| format!("KB catalog metadata delete failed: {}", e))
    }

    fn kb_catalog_subject(kb_id: &str) -> String {
        format!(
            "https://agentos.ontology/kb/{}",
            RdfMapper::sanitize_id(kb_id)
        )
    }

    /// Executes a SPARQL query exclusively against the graph minted from
    /// verified tenant and project claims.
    ///
    /// Queries containing `GRAPH` are rejected instead of trusting a
    /// caller-supplied graph target. This makes the scope non-bypassable and
    /// prevents `GRAPH ?g` from enumerating another tenant's graph.
    pub fn query_sparql_for_claims(
        &self,
        claims: &IsolationClaims,
        sparql: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        if sparql.to_uppercase().contains("GRAPH") {
            return Err(
                "scoped SPARQL queries must not contain GRAPH; the claims graph is applied automatically"
                    .to_string(),
            );
        }
        let graph = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {}", e))?;
        self.query_sparql_in_graph(sparql, Some(&graph))
    }

    /// Writes a graph-local mutation to a graph minted from verified claims.
    ///
    /// The mutation payload cannot name a graph. This prevents callers from
    /// turning an otherwise claims-scoped write into a cross-tenant write.
    pub fn update_for_claims(
        &self,
        claims: &IsolationClaims,
        update: &ClaimsGraphUpdate,
    ) -> Result<(), String> {
        let graph = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {}", e))?;
        self.update_in_claims_graph(&graph, update)
    }

    /// Writes a graph-local mutation to a staging graph derived from verified
    /// claims. `staging_id` is an opaque, safe invocation identifier.
    pub fn update_staging_for_claims(
        &self,
        claims: &IsolationClaims,
        staging_id: &str,
        update: &ClaimsGraphUpdate,
    ) -> Result<(), String> {
        let graph = self.staging_graph_iri_for_claims(claims, staging_id)?;
        self.update_in_claims_graph(&graph, update)
    }

    /// Queries only the staging graph derived from verified claims.
    pub fn query_staging_for_claims(
        &self,
        claims: &IsolationClaims,
        staging_id: &str,
        sparql: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        if sparql.to_uppercase().contains("GRAPH") {
            return Err("claims-scoped staging queries must not contain GRAPH".to_string());
        }
        let graph = self.staging_graph_iri_for_claims(claims, staging_id)?;
        self.query_sparql_in_graph(sparql, Some(&graph))
    }

    /// Merges the staging graph derived from verified claims into its minted
    /// production graph. Neither graph is caller-selectable.
    pub fn commit_staging_for_claims(
        &self,
        claims: &IsolationClaims,
        staging_id: &str,
    ) -> Result<(), String> {
        let staging = self.staging_graph_iri_for_claims(claims, staging_id)?;
        let production = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {}", e))?;
        self.store
            .update(&format!("ADD SILENT GRAPH <{staging}> TO <{production}>"))
            .map_err(|e| format!("claims-scoped staging merge failed: {}", e))
    }

    /// Removes the staging graph derived from verified claims.
    pub fn drop_staging_for_claims(
        &self,
        claims: &IsolationClaims,
        staging_id: &str,
    ) -> Result<(), String> {
        let staging = self.staging_graph_iri_for_claims(claims, staging_id)?;
        self.store
            .update(&format!("DROP SILENT GRAPH <{staging}>"))
            .map_err(|e| format!("claims-scoped staging cleanup failed: {}", e))
    }

    /// Returns the opaque staging graph IRI derived from a minted production
    /// graph. Callers cannot provide a complete graph IRI.
    pub fn staging_graph_iri_for_claims(
        &self,
        claims: &IsolationClaims,
        staging_id: &str,
    ) -> Result<String, String> {
        if staging_id.is_empty()
            || !staging_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(
                "staging identifier must contain only ASCII letters, digits, '-' or '_'"
                    .to_string(),
            );
        }
        let production = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {}", e))?;
        Ok(format!("{production}/staging/{staging_id}"))
    }

    /// Creates a pending approval record in a dedicated graph derived only
    /// from verified tenant/project claims.
    pub fn create_action_approval_for_claims(
        &self,
        claims: &IsolationClaims,
        approval: &PendingActionApproval,
    ) -> Result<(), String> {
        Self::validate_opaque_id(&approval.approval_id, "approval identifier")?;
        Self::validate_opaque_id(&approval.staging_id, "staging identifier")?;
        let expected_staging = self.staging_graph_iri_for_claims(claims, &approval.staging_id)?;
        if approval.staging_graph != expected_staging {
            return Err("approval staging graph is not claims-derived".to_string());
        }
        let graph = self.approvals_graph_iri_for_claims(claims)?;
        let subject = format!("{APPROVAL_BASE_IRI}{}", approval.approval_id);
        let lit = |value: &str| Self::sparql_literal(value);
        let update = format!(
            "INSERT DATA {{ GRAPH <{graph}> {{ \
             <{subject}> <{vocab}approvalId> {id} ; \
             <{vocab}stagingId> {staging_id} ; \
             <{vocab}stagingGraph> {staging_graph} ; \
             <{vocab}actionId> {action_id} ; \
             <{vocab}createdAt> {created_at} ; \
             <{vocab}expiresAt> {expires_at} . \
             }} }}",
            vocab = APPROVAL_VOCAB_IRI,
            id = lit(&approval.approval_id),
            staging_id = lit(&approval.staging_id),
            staging_graph = lit(&approval.staging_graph),
            action_id = lit(&approval.action_id),
            created_at = lit(&approval.created_at),
            expires_at = lit(&approval.expires_at),
        );
        self.store
            .update(&update)
            .map_err(|e| format!("claims-scoped approval create failed: {e}"))
    }

    /// Lists pending approvals visible only in the verified claims scope.
    pub fn list_action_approvals_for_claims(
        &self,
        claims: &IsolationClaims,
    ) -> Result<Vec<PendingActionApproval>, String> {
        let graph = self.approvals_graph_iri_for_claims(claims)?;
        let query = format!(
            "SELECT ?id ?staging_id ?staging_graph ?action_id ?created_at ?expires_at WHERE {{ \
             GRAPH <{graph}> {{ \
             ?approval <{vocab}approvalId> ?id ; \
                 <{vocab}stagingId> ?staging_id ; \
                 <{vocab}stagingGraph> ?staging_graph ; \
                 <{vocab}actionId> ?action_id ; \
                 <{vocab}createdAt> ?created_at ; \
                 <{vocab}expiresAt> ?expires_at . \
             }} }} ORDER BY ?created_at",
            vocab = APPROVAL_VOCAB_IRI,
        );
        self.query_sparql_in_graph(&query, None)?
            .into_iter()
            .map(|row| {
                let get = |name: &str| {
                    row.get(name)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| format!("approval query missing {name}"))
                };
                Ok(PendingActionApproval {
                    approval_id: get("?id")?,
                    staging_id: get("?staging_id")?,
                    staging_graph: get("?staging_graph")?,
                    action_id: get("?action_id")?,
                    created_at: get("?created_at")?,
                    expires_at: get("?expires_at")?,
                })
            })
            .collect()
    }

    /// Removes the approval metadata after an approve, reject, or TTL expiry.
    pub fn delete_action_approval_for_claims(
        &self,
        claims: &IsolationClaims,
        approval_id: &str,
    ) -> Result<(), String> {
        Self::validate_opaque_id(approval_id, "approval identifier")?;
        let graph = self.approvals_graph_iri_for_claims(claims)?;
        let subject = format!("{APPROVAL_BASE_IRI}{approval_id}");
        self.store
            .update(&format!(
                "DELETE WHERE {{ GRAPH <{graph}> {{ <{subject}> ?p ?o }} }}"
            ))
            .map_err(|e| format!("claims-scoped approval cleanup failed: {e}"))
    }

    fn approvals_graph_iri_for_claims(&self, claims: &IsolationClaims) -> Result<String, String> {
        let production = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {e}"))?;
        Ok(format!("{production}/action-approvals"))
    }

    fn validate_opaque_id(value: &str, kind: &str) -> Result<(), String> {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(format!(
                "{kind} must contain only ASCII letters, digits, '-' or '_'"
            ));
        }
        Ok(())
    }

    fn sparql_literal(value: &str) -> String {
        format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
        )
    }

    fn update_in_claims_graph(
        &self,
        graph: &str,
        update: &ClaimsGraphUpdate,
    ) -> Result<(), String> {
        let payload = match update {
            ClaimsGraphUpdate::InsertData(triples) => triples,
            ClaimsGraphUpdate::DeleteWhere(pattern) => pattern,
        };
        if payload.to_uppercase().contains("GRAPH") {
            return Err("claims-scoped updates must not contain GRAPH".to_string());
        }
        let sparql = match update {
            ClaimsGraphUpdate::InsertData(triples) => {
                format!("INSERT DATA {{ GRAPH <{graph}> {{ {triples} }} }}")
            }
            ClaimsGraphUpdate::DeleteWhere(pattern) => {
                format!("DELETE WHERE {{ GRAPH <{graph}> {{ {pattern} }} }}")
            }
        };
        self.store
            .update(&sparql)
            .map_err(|e| format!("claims-scoped SPARQL update failed: {}", e))
    }

    /// Deletes are only permitted through
    /// [`Self::delete_quads_for_source_for_claims`].
    #[deprecated(
        note = "graph deletes require delete_quads_for_source_for_claims with verified claims"
    )]
    pub fn delete_quads_for_source(&self, source_file: &str, graph: &str) -> Result<usize, String> {
        #[cfg(test)]
        return self.delete_quads_for_source_in_graph(source_file, graph);
        #[cfg(not(test))]
        let _ = (source_file, graph);
        Err("verified isolation claims are required for graph deletes".to_string())
    }

    /// Deletes source quads from the graph minted from verified claims.
    pub fn delete_quads_for_source_for_claims(
        &self,
        claims: &IsolationClaims,
        source_file: &str,
    ) -> Result<usize, String> {
        let graph = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {}", e))?;
        self.delete_quads_for_source_in_graph(source_file, &graph)
    }

    fn delete_quads_for_source_in_graph(
        &self,
        source_file: &str,
        graph: &str,
    ) -> Result<usize, String> {
        let safe_file = RdfMapper::sanitize_id(source_file);
        let subject_iri = format!("iri://entity/file:{}", safe_file);
        let delete_sparql = format!(
            "DELETE WHERE {{ GRAPH <{}> {{ <{}> ?p ?o . }} }}",
            graph, subject_iri
        );
        self.store
            .update(&delete_sparql)
            .map_err(|e| format!("SPARQL DELETE failed: {}", e))?;

        let related_delete = format!(
            "DELETE WHERE {{ GRAPH <{}> {{ ?s <https://agentos.ontology/code/contains> <{}> . }} }}",
            graph, subject_iri
        );
        let _ = self.store.update(&related_delete);

        Ok(0)
    }

    /// Deletes are only permitted through
    /// [`Self::delete_quads_by_subject_prefix_for_claims`].
    #[deprecated(
        note = "graph deletes require delete_quads_by_subject_prefix_for_claims with verified claims"
    )]
    pub fn delete_quads_by_subject_prefix(
        &self,
        prefix: &str,
        graph: &str,
    ) -> Result<usize, String> {
        #[cfg(test)]
        return self.delete_quads_by_subject_prefix_in_graph(prefix, graph);
        #[cfg(not(test))]
        let _ = (prefix, graph);
        Err("verified isolation claims are required for graph deletes".to_string())
    }

    /// Deletes matching quads from the graph minted from verified claims.
    pub fn delete_quads_by_subject_prefix_for_claims(
        &self,
        claims: &IsolationClaims,
        prefix: &str,
    ) -> Result<usize, String> {
        let graph = claims
            .graph_iri()
            .map_err(|e| format!("invalid verified graph scope: {}", e))?;
        self.delete_quads_by_subject_prefix_in_graph(prefix, &graph)
    }

    fn delete_quads_by_subject_prefix_in_graph(
        &self,
        prefix: &str,
        graph: &str,
    ) -> Result<usize, String> {
        let sparql = format!(
            "SELECT DISTINCT ?s WHERE {{ GRAPH <{}> {{ ?s ?p ?o . FILTER(STRSTARTS(STR(?s), \"{}\")) }} }}",
            graph, Self::escape_sparql_string(prefix)
        );

        let subjects: Vec<String> = match self.store.query(&sparql) {
            Ok(QueryResults::Solutions(solutions)) => solutions
                .filter_map(|sol| sol.ok())
                .filter_map(|sol| {
                    sol.get(0).map(|v| {
                        v.to_string()
                            .trim_start_matches('<')
                            .trim_end_matches('>')
                            .to_string()
                    })
                })
                .collect(),
            _ => return Ok(0),
        };

        let count = subjects.len();
        for subject in &subjects {
            let s = format!("<{}>", subject);
            let del = format!("DELETE WHERE {{ GRAPH <{}> {{ {} ?p ?o . }} }}", graph, s);
            let _ = self.store.update(&del);
            let del_in = format!("DELETE WHERE {{ GRAPH <{}> {{ ?s ?p {} . }} }}", graph, s);
            let _ = self.store.update(&del_in);
        }

        Ok(count)
    }

    /// Reads without a verified graph scope are rejected.
    #[deprecated(note = "graph reads require query_sparql_for_claims with verified claims")]
    pub fn query_sparql(
        &self,
        sparql: &str,
        named_graph: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, String> {
        #[cfg(test)]
        return self.query_sparql_in_graph(sparql, named_graph);
        #[cfg(not(test))]
        let _ = (sparql, named_graph);
        Err("verified isolation claims are required for graph reads".to_string())
    }

    fn query_sparql_in_graph(
        &self,
        sparql: &str,
        named_graph: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let final_sparql = match named_graph {
            Some(graph) if !sparql.to_uppercase().contains("GRAPH") => {
                let g = format!("<{}>", graph);
                Self::wrap_in_named_graph(sparql, &g)
            }
            _ => sparql.to_string(),
        };

        let results = self
            .store
            .query(&final_sparql)
            .map_err(|e| format!("SPARQL query failed: {}", e))?;

        let mut values = Vec::new();
        match results {
            QueryResults::Solutions(solutions) => {
                for solution in solutions {
                    let solution =
                        solution.map_err(|e| format!("Failed to read query result: {}", e))?;
                    let mut obj = serde_json::Map::new();
                    for (var, value) in solution.iter() {
                        obj.insert(
                            var.to_string(),
                            serde_json::Value::String(normalize_term(&value.to_string())),
                        );
                    }
                    values.push(serde_json::Value::Object(obj));
                }
            }
            QueryResults::Graph(graph) => {
                for triple in graph {
                    let triple =
                        triple.map_err(|e| format!("Failed to read graph result: {}", e))?;
                    let mut obj = serde_json::Map::new();
                    obj.insert(
                        "subject".to_string(),
                        serde_json::Value::String(triple.subject.to_string()),
                    );
                    obj.insert(
                        "predicate".to_string(),
                        serde_json::Value::String(triple.predicate.to_string()),
                    );
                    obj.insert(
                        "object".to_string(),
                        serde_json::Value::String(triple.object.to_string()),
                    );
                    values.push(serde_json::Value::Object(obj));
                }
            }
            QueryResults::Boolean(b) => {
                values.push(serde_json::json!({"result": b}));
            }
        }
        Ok(values)
    }

    /// 将不含 GRAPH 子句的 SPARQL 查询包装到指定命名图中。
    /// 通过大括号配对定位 WHERE 块的真实结束位置，使 LIMIT / ORDER BY / GROUP BY
    /// 等尾部求解修饰符保留在 GRAPH 包装之外，避免拼出非法语法。
    fn wrap_in_named_graph(sparql: &str, g: &str) -> String {
        let upper = sparql.to_uppercase();
        // SELECT normally has WHERE; ASK may omit it (`ASK { ... }`). Preserve the
        // query form in both cases instead of turning an ASK into invalid SELECT text.
        let open_abs = match upper.find("WHERE") {
            Some(where_pos) => {
                let after_where = &sparql[where_pos + 5..];
                match after_where.find('{') {
                    Some(open_rel) => where_pos + 5 + open_rel,
                    None => return sparql.to_string(),
                }
            }
            None => match sparql.find('{') {
                Some(open_pos) => open_pos,
                None => return sparql.to_string(),
            },
        };

        // 大括号配对，找到与之匹配的 '}'
        let bytes = sparql.as_bytes();
        let mut depth = 0usize;
        let mut close_abs = None;
        for (i, &b) in bytes.iter().enumerate().skip(open_abs) {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close_abs = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close_abs = match close_abs {
            Some(p) => p,
            None => return sparql.to_string(),
        };

        let prefix = &sparql[..=open_abs]; // 含 WHERE 的左大括号
        let inner = sparql[open_abs + 1..close_abs].trim();
        let suffix = sparql[close_abs + 1..].trim(); // LIMIT / ORDER BY 等

        if suffix.is_empty() {
            format!("{} GRAPH {} {{ {} }} }}", prefix, g, inner)
        } else {
            format!("{} GRAPH {} {{ {} }} }} {}", prefix, g, inner, suffix)
        }
    }

    pub fn search_entities(
        &self,
        keyword: &str,
        entity_type: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, String> {
        #[cfg(test)]
        {
            let escaped = Self::escape_sparql_string(keyword);
            let type_filter = entity_type.map_or_else(String::new, |entity_type| {
                format!(
                    "?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{}> .",
                    entity_type
                )
            });
            let sparql = format!(
                "SELECT DISTINCT ?s ?label WHERE {{ GRAPH ?g {{
                    ?s <http://www.w3.org/2000/01/rdf-schema#label> ?label .
                    {}
                    FILTER(CONTAINS(LCASE(STR(?label)), LCASE(\"{}\")))
                }} }}",
                type_filter, escaped
            );
            return self.query_sparql_in_graph(&sparql, None);
        }
        #[cfg(not(test))]
        let _ = (keyword, entity_type);
        Err("verified isolation claims are required for graph reads".to_string())
    }

    /// Searches entities in the graph minted from verified claims only.
    pub fn search_entities_for_claims(
        &self,
        claims: &IsolationClaims,
        keyword: &str,
        entity_type: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let escaped = Self::escape_sparql_string(keyword);
        let type_filter = entity_type.map_or_else(String::new, |entity_type| {
            format!(
                "?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{}> .",
                entity_type
            )
        });
        self.query_sparql_for_claims(
            claims,
            &format!(
                "SELECT DISTINCT ?s ?label WHERE {{
                    ?s <http://www.w3.org/2000/01/rdf-schema#label> ?label .
                    {}
                    FILTER(CONTAINS(LCASE(STR(?label)), LCASE(\"{}\")))
                }}",
                type_filter, escaped
            ),
        )
    }

    pub fn get_neighbors(
        &self,
        entity_id: &str,
        depth: usize,
    ) -> Result<serde_json::Value, String> {
        #[cfg(test)]
        {
            if depth == 0 || depth > 3 {
                return Ok(serde_json::json!({
                    "entity": entity_id, "neighbors": [], "depth": depth
                }));
            }
            let mut all_neighbors = Vec::new();
            let mut visited = std::collections::HashSet::from([entity_id.to_string()]);
            let mut current_level = vec![entity_id.to_string()];
            for level in 0..depth {
                let mut next_level = Vec::new();
                for node_id in &current_level {
                    let node = format!("<{}>", node_id);
                    for row in self.query_sparql_in_graph(
                        &format!("SELECT ?p ?o WHERE {{ GRAPH ?g {{ {} ?p ?o . }} }}", node),
                        None,
                    )? {
                        if let (Some(pred), Some(obj)) = (
                            row.get("?p").and_then(|v| v.as_str()),
                            row.get("?o").and_then(|v| v.as_str()),
                        ) {
                            let obj_clean = obj.trim_start_matches('<').trim_end_matches('>');
                            all_neighbors.push(serde_json::json!({
                                "source": node_id, "predicate": pred, "target": obj_clean,
                                "direction": "outgoing", "level": level + 1
                            }));
                            if visited.insert(obj_clean.to_string()) && level + 1 < depth {
                                next_level.push(obj_clean.to_string());
                            }
                        }
                    }
                    for row in self.query_sparql_in_graph(
                        &format!("SELECT ?s ?p WHERE {{ GRAPH ?g {{ ?s ?p {} . }} }}", node),
                        None,
                    )? {
                        if let (Some(subj), Some(pred)) = (
                            row.get("?s").and_then(|v| v.as_str()),
                            row.get("?p").and_then(|v| v.as_str()),
                        ) {
                            let subj_clean = subj.trim_start_matches('<').trim_end_matches('>');
                            all_neighbors.push(serde_json::json!({
                                "source": subj_clean, "predicate": pred, "target": node_id,
                                "direction": "incoming", "level": level + 1
                            }));
                            if visited.insert(subj_clean.to_string()) && level + 1 < depth {
                                next_level.push(subj_clean.to_string());
                            }
                        }
                    }
                }
                current_level = next_level;
            }
            return Ok(serde_json::json!({
                "entity": entity_id, "neighbors": all_neighbors, "depth": depth,
                "total_found": all_neighbors.len()
            }));
        }
        #[cfg(not(test))]
        let _ = (entity_id, depth);
        Err("verified isolation claims are required for graph reads".to_string())
    }

    /// Traverses neighbours in the graph minted from verified claims only.
    pub fn get_neighbors_for_claims(
        &self,
        claims: &IsolationClaims,
        entity_id: &str,
        depth: usize,
    ) -> Result<serde_json::Value, String> {
        if depth == 0 || depth > 3 {
            return Ok(serde_json::json!({
                "entity": entity_id,
                "neighbors": [],
                "depth": depth
            }));
        }

        let mut all_neighbors = Vec::new();
        let mut visited = std::collections::HashSet::from([entity_id.to_string()]);
        let mut current_level = vec![entity_id.to_string()];

        for level in 0..depth {
            let mut next_level = Vec::new();
            for node_id in &current_level {
                let node = format!("<{}>", node_id);
                let out_sparql = format!("SELECT ?p ?o WHERE {{ {} ?p ?o . }}", node);
                for row in self.query_sparql_for_claims(claims, &out_sparql)? {
                    if let (Some(pred), Some(obj)) = (
                        row.get("?p").and_then(|v| v.as_str()),
                        row.get("?o").and_then(|v| v.as_str()),
                    ) {
                        let obj_clean = obj.trim_start_matches('<').trim_end_matches('>');
                        all_neighbors.push(serde_json::json!({
                            "source": node_id, "predicate": pred, "target": obj_clean,
                            "direction": "outgoing", "level": level + 1
                        }));
                        if visited.insert(obj_clean.to_string()) && level + 1 < depth {
                            next_level.push(obj_clean.to_string());
                        }
                    }
                }

                let in_sparql = format!("SELECT ?s ?p WHERE {{ ?s ?p {} . }}", node);
                for row in self.query_sparql_for_claims(claims, &in_sparql)? {
                    if let (Some(subj), Some(pred)) = (
                        row.get("?s").and_then(|v| v.as_str()),
                        row.get("?p").and_then(|v| v.as_str()),
                    ) {
                        let subj_clean = subj.trim_start_matches('<').trim_end_matches('>');
                        all_neighbors.push(serde_json::json!({
                            "source": subj_clean, "predicate": pred, "target": node_id,
                            "direction": "incoming", "level": level + 1
                        }));
                        if visited.insert(subj_clean.to_string()) && level + 1 < depth {
                            next_level.push(subj_clean.to_string());
                        }
                    }
                }
            }
            current_level = next_level;
        }

        Ok(serde_json::json!({
            "entity": entity_id,
            "neighbors": all_neighbors,
            "depth": depth,
            "total_found": all_neighbors.len()
        }))
    }

    fn escape_sparql_string(s: &str) -> String {
        let mut escaped = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\\' => escaped.push_str("\\\\"),
                '"' => escaped.push_str("\\\""),
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                '\t' => escaped.push_str("\\t"),
                c if c.is_control() => {
                    escaped.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => escaped.push(c),
            }
        }
        escaped
    }

    pub fn default_graph(&self) -> &str {
        &self.default_graph
    }

    // Knowledge graph → vector index sync is handled externally via HyperspaceEngine.
    // See src/memory/hyperspace_store.rs for the vector store API.
    // To index KG entities into vector search, iterate results from SPARQL queries
    // and call HyperspaceStore::upsert() with the entity IRI and text content.
}

fn normalize_term(s: &str) -> String {
    if s.starts_with('<') && s.ends_with('>') {
        s[1..s.len() - 1].to_string()
    } else if s.starts_with('"') {
        if let Some(pos) = s.rfind("\"^^<") {
            s[1..pos].to_string()
        } else if let Some(pos) = s.rfind("\"@") {
            s[1..pos].to_string()
        } else if s.ends_with('"') && s.len() > 1 {
            s[1..s.len() - 1].to_string()
        } else {
            s.to_string()
        }
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge_graph::types::RdfValue;

    static RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    static RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
    static PERSON: &str = "http://example.org/Person";
    static VEHICLE: &str = "http://example.org/Vehicle";
    static KNOWS: &str = "http://example.org/knows";
    static TEST_GRAPH: &str = "http://test/graph";

    fn claims() -> IsolationClaims {
        IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap()
    }

    fn make_quad(s: &str, p: &str, o: RdfValue) -> RdfQuad {
        RdfQuad {
            subject: s.to_string(),
            predicate: p.to_string(),
            object: o,
            graph: Some(TEST_GRAPH.to_string()),
        }
    }

    #[test]
    fn test_write_and_query_quads() {
        let store = KnowledgeGraphStore::new().unwrap();

        let quads = vec![
            make_quad(
                "http://example.org/alice",
                RDF_TYPE,
                RdfValue::Iri(PERSON.to_string()),
            ),
            make_quad(
                "http://example.org/alice",
                RDFS_LABEL,
                RdfValue::Literal("Alice".to_string()),
            ),
            make_quad(
                "http://example.org/alice",
                "http://example.org/age",
                RdfValue::TypedLiteral(
                    "30".to_string(),
                    "http://www.w3.org/2001/XMLSchema#integer".to_string(),
                ),
            ),
        ];

        store.write_quads_for_claims(&claims(), &quads).unwrap();

        let results = store
            .query_sparql_for_claims(&claims(), "SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
            .unwrap();

        assert_eq!(results.len(), 3, "should return 3 triples");

        let labels: Vec<&str> = results
            .iter()
            .filter_map(|r| {
                if r.get("?p").and_then(|v| v.as_str()) == Some(RDFS_LABEL) {
                    r.get("?o").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0], "Alice");
    }

    #[test]
    fn test_write_empty_quads() {
        let store = KnowledgeGraphStore::new().unwrap();
        let result = store.write_quads_for_claims(&claims(), &[]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_search_entities() {
        let store = KnowledgeGraphStore::new().unwrap();

        let quads = vec![
            make_quad(
                "http://example.org/alice",
                RDF_TYPE,
                RdfValue::Iri(PERSON.to_string()),
            ),
            make_quad(
                "http://example.org/alice",
                RDFS_LABEL,
                RdfValue::Literal("Alice Johnson".to_string()),
            ),
            make_quad(
                "http://example.org/bob",
                RDF_TYPE,
                RdfValue::Iri(PERSON.to_string()),
            ),
            make_quad(
                "http://example.org/bob",
                RDFS_LABEL,
                RdfValue::Literal("Bob Smith".to_string()),
            ),
            make_quad(
                "http://example.org/car",
                RDF_TYPE,
                RdfValue::Iri(VEHICLE.to_string()),
            ),
            make_quad(
                "http://example.org/car",
                RDFS_LABEL,
                RdfValue::Literal("Toyota Car".to_string()),
            ),
        ];

        store.write_quads_for_claims(&claims(), &quads).unwrap();

        let results = store
            .search_entities_for_claims(&claims(), "alice", None)
            .unwrap();
        assert_eq!(results.len(), 1, "fuzzy search should find Alice");
        let label = results[0].get("?label").and_then(|v| v.as_str()).unwrap();
        assert!(label.contains("Alice"));

        let person_results = store
            .search_entities_for_claims(&claims(), "o", Some(PERSON))
            .unwrap();
        assert!(
            person_results.len() >= 2,
            "search by type Person should find at least 2"
        );

        let vehicle_results = store
            .search_entities_for_claims(&claims(), "alice", Some(VEHICLE))
            .unwrap();
        assert_eq!(vehicle_results.len(), 0, "Alice is not Vehicle type");
    }

    #[test]
    fn test_search_entities_case_insensitive() {
        let store = KnowledgeGraphStore::new().unwrap();

        let quads = vec![make_quad(
            "http://example.org/alice",
            RDFS_LABEL,
            RdfValue::Literal("Alice".to_string()),
        )];

        store.write_quads_for_claims(&claims(), &quads).unwrap();

        let upper = store
            .search_entities_for_claims(&claims(), "ALICE", None)
            .unwrap();
        assert_eq!(
            upper.len(),
            1,
            "case-insensitive search should find results"
        );

        let lower = store
            .search_entities_for_claims(&claims(), "alice", None)
            .unwrap();
        assert_eq!(lower.len(), 1);

        let mixed = store
            .search_entities_for_claims(&claims(), "AlIcE", None)
            .unwrap();
        assert_eq!(mixed.len(), 1);
    }

    #[test]
    fn test_get_neighbors() {
        let store = KnowledgeGraphStore::new().unwrap();

        let quads = vec![
            make_quad(
                "http://example.org/alice",
                RDF_TYPE,
                RdfValue::Iri(PERSON.to_string()),
            ),
            make_quad(
                "http://example.org/alice",
                RDFS_LABEL,
                RdfValue::Literal("Alice".to_string()),
            ),
            make_quad(
                "http://example.org/alice",
                KNOWS,
                RdfValue::Iri("http://example.org/bob".to_string()),
            ),
            make_quad(
                "http://example.org/bob",
                RDF_TYPE,
                RdfValue::Iri(PERSON.to_string()),
            ),
            make_quad(
                "http://example.org/bob",
                RDFS_LABEL,
                RdfValue::Literal("Bob".to_string()),
            ),
            make_quad(
                "http://example.org/bob",
                KNOWS,
                RdfValue::Iri("http://example.org/charlie".to_string()),
            ),
            make_quad(
                "http://example.org/charlie",
                RDFS_LABEL,
                RdfValue::Literal("Charlie".to_string()),
            ),
        ];

        store.write_quads_for_claims(&claims(), &quads).unwrap();

        let result = store
            .get_neighbors_for_claims(&claims(), "http://example.org/alice", 1)
            .unwrap();

        let neighbors = result.get("neighbors").unwrap().as_array().unwrap();
        assert!(
            neighbors.len() >= 2,
            "1-hop traversal should find at least 2 neighbors (type + label + knows)"
        );

        let knows: Vec<_> = neighbors
            .iter()
            .filter(|n| n.get("predicate").and_then(|v| v.as_str()) == Some(KNOWS))
            .collect();
        assert_eq!(knows.len(), 1, "should find 1 knows relation");
        assert_eq!(
            knows[0].get("target").and_then(|v| v.as_str()),
            Some("http://example.org/bob")
        );
    }

    #[test]
    fn test_get_neighbors_depth_2() {
        let store = KnowledgeGraphStore::new().unwrap();

        let quads = vec![
            make_quad(
                "http://example.org/alice",
                KNOWS,
                RdfValue::Iri("http://example.org/bob".to_string()),
            ),
            make_quad(
                "http://example.org/bob",
                KNOWS,
                RdfValue::Iri("http://example.org/charlie".to_string()),
            ),
        ];

        store.write_quads_for_claims(&claims(), &quads).unwrap();

        let result = store
            .get_neighbors_for_claims(&claims(), "http://example.org/alice", 2)
            .unwrap();

        let neighbors = result.get("neighbors").unwrap().as_array().unwrap();
        assert!(
            neighbors.len() >= 2,
            "2-hop traversal should find alice->bob and bob->charlie"
        );

        let targets: Vec<_> = neighbors
            .iter()
            .filter_map(|n| {
                if n.get("predicate").and_then(|v| v.as_str()) == Some(KNOWS) {
                    n.get("target").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(
            targets.contains(&"http://example.org/bob"),
            "should include bob as direct neighbor"
        );
        assert!(
            targets.contains(&"http://example.org/charlie"),
            "should include charlie as 2-hop neighbor"
        );
    }

    #[test]
    fn test_get_neighbors_zero_depth() {
        let store = KnowledgeGraphStore::new().unwrap();
        let result = store
            .get_neighbors_for_claims(&claims(), "http://example.org/alice", 0)
            .unwrap();
        let neighbors = result.get("neighbors").unwrap().as_array().unwrap();
        assert!(
            neighbors.is_empty(),
            "depth=0 should return empty neighbor list"
        );
    }

    #[test]
    fn test_with_graph_constructor() {
        let store = KnowledgeGraphStore::with_graph("http://test/custom").unwrap();
        assert_eq!(store.default_graph(), "http://test/custom");
    }

    #[test]
    fn test_default_graph() {
        let store = KnowledgeGraphStore::new().unwrap();
        assert_eq!(store.default_graph(), "graph:world");
    }

    #[test]
    fn test_query_sparql_no_named_graph() {
        let store = KnowledgeGraphStore::new().unwrap();

        let quads = vec![make_quad(
            "http://example.org/x",
            RDFS_LABEL,
            RdfValue::Literal("X".to_string()),
        )];

        store.write_quads_for_claims(&claims(), &quads).unwrap();

        let result = store
            .query_sparql_for_claims(&claims(), "SELECT ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }");
        assert!(result.is_err(), "claims-scoped queries must reject GRAPH");
    }

    #[test]
    fn test_query_sparql_with_graph_clause() {
        let store = KnowledgeGraphStore::new().unwrap();

        let quads = vec![make_quad(
            "http://example.org/x",
            RDFS_LABEL,
            RdfValue::Literal("X".to_string()),
        )];

        store.write_quads_for_claims(&claims(), &quads).unwrap();

        let results = store
            .query_sparql_for_claims(&claims(), "SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "should not double-wrap when GRAPH clause already exists"
        );
    }

    #[test]
    fn test_query_sparql_named_graph_with_limit() {
        let store = KnowledgeGraphStore::new().unwrap();

        for i in 0..3 {
            store
                .write_quads_for_claims(
                    &claims(),
                    &[make_quad(
                        &format!("http://example.org/n{}", i),
                        RDFS_LABEL,
                        RdfValue::Literal(format!("N{}", i)),
                    )],
                )
                .unwrap();
        }

        // 带 LIMIT 的查询不得把 LIMIT 包进 GRAPH 大括号内（回归：expected OPTIONAL）
        let results = store
            .query_sparql_for_claims(&claims(), "SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 2")
            .unwrap();
        assert_eq!(results.len(), 2, "LIMIT 应保留在 GRAPH 包装之外并生效");
    }

    #[test]
    fn test_incoming_neighbors() {
        let store = KnowledgeGraphStore::new().unwrap();

        let quads = vec![make_quad(
            "http://example.org/alice",
            KNOWS,
            RdfValue::Iri("http://example.org/bob".to_string()),
        )];

        store.write_quads_for_claims(&claims(), &quads).unwrap();

        let result = store
            .get_neighbors_for_claims(&claims(), "http://example.org/bob", 1)
            .unwrap();

        let neighbors = result.get("neighbors").unwrap().as_array().unwrap();
        let incoming: Vec<_> = neighbors
            .iter()
            .filter(|n| n.get("direction").and_then(|v| v.as_str()) == Some("incoming"))
            .collect();
        assert_eq!(
            incoming.len(),
            1,
            "bob should have 1 incoming edge (from alice)"
        );
        assert_eq!(
            incoming[0].get("source").and_then(|v| v.as_str()),
            Some("http://example.org/alice")
        );
    }

    #[test]
    fn claims_scope_new_writes_and_preserves_legacy_world_graph() {
        let store = KnowledgeGraphStore::new().unwrap();
        let tenant_a = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        let tenant_b = IsolationClaims::from_verified("tenant-b", "project-b", "actor-b").unwrap();

        // This is historical data, not a migration target for claims-scoped
        // reads or writes.
        store
            .store
            .update(
                "INSERT DATA { GRAPH <graph:world> {
                    <http://example.org/legacy> <http://example.org/name> \"legacy\" .
                }}",
            )
            .unwrap();

        let quad = make_quad(
            "http://example.org/tenant-a",
            RDFS_LABEL,
            RdfValue::Literal("Tenant A only".to_string()),
        );
        store.write_quads_for_claims(&tenant_a, &[quad]).unwrap();

        let a_results = store
            .query_sparql_for_claims(
                &tenant_a,
                "SELECT ?s WHERE { ?s <http://www.w3.org/2000/01/rdf-schema#label> \"Tenant A only\" }",
            )
            .unwrap();
        assert_eq!(a_results.len(), 1);

        let b_results = store
            .query_sparql_for_claims(
                &tenant_b,
                "SELECT ?s WHERE { ?s <http://www.w3.org/2000/01/rdf-schema#label> \"Tenant A only\" }",
            )
            .unwrap();
        assert!(
            b_results.is_empty(),
            "tenant B must not see tenant A's newly written triples"
        );

        let legacy_results = store
            .store
            .query(
                "SELECT ?s WHERE {
                    GRAPH <graph:world> { ?s <http://example.org/name> \"legacy\" }
                }",
            )
            .unwrap();
        let QueryResults::Solutions(legacy_results) = legacy_results else {
            panic!("expected legacy graph query to return solutions");
        };
        assert_eq!(
            legacy_results.count(),
            1,
            "legacy graph:world remains intact"
        );
    }

    #[test]
    fn claims_scoped_ask_query_keeps_ask_form() {
        let query = KnowledgeGraphStore::wrap_in_named_graph(
            "ASK { ?s ?p ?o }",
            "<graph://tenant-a/project-a/staging/test>",
        );
        assert_eq!(
            query,
            "ASK { GRAPH <graph://tenant-a/project-a/staging/test> { ?s ?p ?o } }"
        );
    }
}
