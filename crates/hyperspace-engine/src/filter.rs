use roaring::RoaringBitmap;

use crate::jsonld_meta::JsonLdMetadataIndex;

/// JSON-LD aware filter expressions for search.
#[derive(Debug, Clone)]
pub enum JsonLdFilter {
    Type(String),
    NamedGraph(String),
    Context(String),
    Tag {
        key: String,
        value: String,
    },
    Range {
        key: String,
        gte: Option<f64>,
        lte: Option<f64>,
    },
    Must(Vec<JsonLdFilter>),
    Should(Vec<JsonLdFilter>),
    MustNot(Vec<JsonLdFilter>),
}

impl JsonLdFilter {
    pub fn tag(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Tag {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// Compiled filter for fast membership testing against bitmaps.
#[derive(Debug, Clone)]
pub enum CompiledFilter {
    MatchTag(String, String),
    MatchType(String),
    MatchGraph(String),
    MatchContext(String),
    Range {
        key: String,
        gte: Option<f64>,
        lte: Option<f64>,
    },
    And(Vec<CompiledFilter>),
    Or(Vec<CompiledFilter>),
    Not(Box<CompiledFilter>),
}

impl CompiledFilter {
    /// Evaluate this filter against the metadata index, returning a RoaringBitmap
    /// of matching IDs. `None` is reserved for callers that provide no filter;
    /// every compiled filter evaluates to a bitmap, including an empty one.
    pub fn evaluate(&self, meta: &JsonLdMetadataIndex) -> Option<RoaringBitmap> {
        match self {
            Self::MatchTag(key, val) => Some(meta.ids_for_tag(key, val).unwrap_or_default()),
            Self::MatchType(typ) => Some(meta.ids_for_type(typ).unwrap_or_default()),
            Self::MatchGraph(g) => Some(meta.ids_for_graph(g).unwrap_or_default()),
            Self::MatchContext(ctx) => Some(meta.ids_for_context(ctx).unwrap_or_default()),
            Self::Range { key, gte, lte } => Some(
                meta.ids_for_numeric_range(key, *gte, *lte)
                    .unwrap_or_default(),
            ),
            Self::And(conds) => {
                // The empty conjunction is the identity: it matches every live ID.
                let mut result: RoaringBitmap = meta.all_ids().iter().collect();
                for cond in conds {
                    let Some(matches) = cond.evaluate(meta) else {
                        return Some(RoaringBitmap::new());
                    };
                    result &= matches;
                    if result.is_empty() {
                        break;
                    }
                }
                Some(result)
            }
            Self::Or(conds) => {
                let mut result = RoaringBitmap::new();
                for c in conds {
                    if let Some(bm) = c.evaluate(meta) {
                        result |= bm;
                    }
                }
                Some(result)
            }
            Self::Not(cond) => {
                // Not is tricky: we need the universe of live IDs minus matching ones
                let all: RoaringBitmap = meta.all_ids().iter().collect();
                if let Some(matching) = cond.evaluate(meta) {
                    Some(all - matching)
                } else {
                    Some(RoaringBitmap::new())
                }
            }
        }
    }

    /// Check if a set of tags/values satisfies this filter (legacy function-based path).
    /// `has_tag` returns true if the key:value pair exists.
    /// `get_numeric` returns numeric value for a key.
    pub fn check(
        &self,
        has_tag: &impl Fn(&str, &str) -> bool,
        get_numeric: &impl Fn(&str) -> Option<f64>,
        has_type: &impl Fn(&str) -> bool,
        has_graph: &impl Fn(&str) -> bool,
    ) -> bool {
        match self {
            Self::MatchTag(key, val) => has_tag(key, val),
            Self::MatchType(typ) => has_type(typ),
            Self::MatchGraph(g) => has_graph(g),
            Self::MatchContext(_) => true,
            Self::Range { key, gte, lte } => {
                if let Some(val) = get_numeric(key) {
                    let ok_ge = gte.is_none_or(|g| val >= g);
                    let ok_le = lte.is_none_or(|l| val <= l);
                    ok_ge && ok_le
                } else {
                    false
                }
            }
            Self::And(conds) => conds
                .iter()
                .all(|c| c.check(has_tag, get_numeric, has_type, has_graph)),
            Self::Or(conds) => conds
                .iter()
                .any(|c| c.check(has_tag, get_numeric, has_type, has_graph)),
            Self::Not(cond) => !cond.check(has_tag, get_numeric, has_type, has_graph),
        }
    }
}

/// Convert JsonLdFilter to CompiledFilter.
pub fn compile_filter(f: &JsonLdFilter) -> CompiledFilter {
    match f {
        JsonLdFilter::Tag { key, value } => CompiledFilter::MatchTag(key.clone(), value.clone()),
        JsonLdFilter::Type(t) => CompiledFilter::MatchType(t.clone()),
        JsonLdFilter::NamedGraph(g) => CompiledFilter::MatchGraph(g.clone()),
        JsonLdFilter::Context(c) => CompiledFilter::MatchContext(c.clone()),
        JsonLdFilter::Range { key, gte, lte } => CompiledFilter::Range {
            key: key.clone(),
            gte: *gte,
            lte: *lte,
        },
        JsonLdFilter::Must(children) => {
            CompiledFilter::And(children.iter().map(compile_filter).collect())
        }
        JsonLdFilter::Should(children) => {
            CompiledFilter::Or(children.iter().map(compile_filter).collect())
        }
        JsonLdFilter::MustNot(children) => {
            let inner: Vec<CompiledFilter> = children.iter().map(compile_filter).collect();
            if inner.len() == 1 {
                CompiledFilter::Not(Box::new(inner.into_iter().next().unwrap()))
            } else {
                CompiledFilter::Not(Box::new(CompiledFilter::And(inner)))
            }
        }
    }
}

/// Evaluate a set of JSON-LD filters against the metadata index.
///
/// An empty filter slice returns `None` (no restriction). A non-empty filter
/// always returns `Some`, including `Some(empty)` when it matches nothing.
pub fn evaluate_filters(
    meta: &JsonLdMetadataIndex,
    filters: &[JsonLdFilter],
) -> Option<RoaringBitmap> {
    if filters.is_empty() {
        return None; // No filter = all IDs (None means "no restriction")
    }
    let compiled: Vec<CompiledFilter> = filters.iter().map(compile_filter).collect();
    let filter = CompiledFilter::And(compiled);
    filter.evaluate(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tag_filter_match() {
        let filter = compile_filter(&JsonLdFilter::tag("tags", "important"));
        let result = filter.check(
            &|k, v| k == "tags" && v == "important",
            &|_| None,
            &|_| false,
            &|_| false,
        );
        assert!(result);
    }

    #[test]
    fn test_tag_filter_no_match() {
        let filter = compile_filter(&JsonLdFilter::tag("tags", "important"));
        let result = filter.check(
            &|k, v| k == "tags" && v == "unimportant",
            &|_| None,
            &|_| false,
            &|_| false,
        );
        assert!(!result);
    }

    #[test]
    fn test_must_all_pass() {
        let must = JsonLdFilter::Must(vec![
            JsonLdFilter::tag("a", "1"),
            JsonLdFilter::tag("b", "2"),
        ]);
        let filter = compile_filter(&must);
        let result = filter.check(
            &|k, v| matches!((k, v), ("a", "1") | ("b", "2")),
            &|_| None,
            &|_| false,
            &|_| false,
        );
        assert!(result);
    }

    #[test]
    fn test_must_fail_if_one_missing() {
        let must = JsonLdFilter::Must(vec![
            JsonLdFilter::tag("a", "1"),
            JsonLdFilter::tag("b", "2"),
        ]);
        let filter = compile_filter(&must);
        let result = filter.check(
            &|k, v| k == "a" && v == "1", // missing b:2
            &|_| None,
            &|_| false,
            &|_| false,
        );
        assert!(!result);
    }

    #[test]
    fn test_should_any_pass() {
        let should = JsonLdFilter::Should(vec![
            JsonLdFilter::tag("x", "1"),
            JsonLdFilter::tag("y", "2"),
        ]);
        let filter = compile_filter(&should);
        let result = filter.check(&|k, v| k == "y" && v == "2", &|_| None, &|_| false, &|_| {
            false
        });
        assert!(result);
    }

    #[test]
    fn test_must_not_excludes() {
        let must_not = JsonLdFilter::MustNot(vec![JsonLdFilter::tag("bad", "true")]);
        let filter = compile_filter(&must_not);
        let result = filter.check(
            &|k, v| k == "bad" && v == "true",
            &|_| None,
            &|_| false,
            &|_| false,
        );
        assert!(!result);
    }

    #[test]
    fn test_range_filter() {
        let range = JsonLdFilter::Range {
            key: "importance".into(),
            gte: Some(0.5),
            lte: None,
        };
        let filter = compile_filter(&range);
        let result = filter.check(
            &|_, _| false,
            &|k| {
                if k == "importance" {
                    Some(0.8)
                } else {
                    None
                }
            },
            &|_| false,
            &|_| false,
        );
        assert!(result);

        let result2 = filter.check(
            &|_, _| false,
            &|k| {
                if k == "importance" {
                    Some(0.2)
                } else {
                    None
                }
            },
            &|_| false,
            &|_| false,
        );
        assert!(!result2);
    }

    // ── Bitmap evaluation tests (require JsonLdMetadataIndex) ──

    use crate::jsonld_meta::JsonLdMetadataIndex;
    use serde_json::json;

    fn setup_test_index() -> JsonLdMetadataIndex {
        let meta = JsonLdMetadataIndex::new();
        meta.index(1, &json!({"@type": ["Document"], "tags": ["important"], "importance": 0.9, "named_graph": "g1"}));
        meta.index(2, &json!({"@type": ["Document", "Report"], "tags": ["normal"], "importance": 0.5, "named_graph": "g1"}));
        meta.index(3, &json!({"@type": ["Note"], "tags": ["important"], "importance": 0.3, "named_graph": "g2"}));
        meta
    }

    #[test]
    fn test_evaluate_tag() {
        let meta = setup_test_index();
        let cf = compile_filter(&JsonLdFilter::tag("tags", "important"));
        let bm = cf.evaluate(&meta).unwrap();
        assert!(bm.contains(1));
        assert!(!bm.contains(2));
        assert!(bm.contains(3));
    }

    #[test]
    fn test_evaluate_type() {
        let meta = setup_test_index();
        let cf = compile_filter(&JsonLdFilter::Type("Report".into()));
        let bm = cf.evaluate(&meta).unwrap();
        assert!(!bm.contains(1));
        assert!(bm.contains(2));
        assert!(!bm.contains(3));
    }

    #[test]
    fn test_evaluate_range() {
        let meta = setup_test_index();
        let cf = compile_filter(&JsonLdFilter::Range {
            key: "importance".into(),
            gte: Some(0.6),
            lte: None,
        });
        let bm = cf.evaluate(&meta).unwrap();
        assert!(bm.contains(1));
        assert!(!bm.contains(2));
        assert!(!bm.contains(3));
    }

    #[test]
    fn test_evaluate_must() {
        let meta = setup_test_index();
        let filter = JsonLdFilter::Must(vec![
            JsonLdFilter::tag("tags", "important"),
            JsonLdFilter::Type("Document".into()),
        ]);
        let cf = compile_filter(&filter);
        let bm = cf.evaluate(&meta).unwrap();
        assert!(bm.contains(1));
        assert!(!bm.contains(2)); // tags=normal
        assert!(!bm.contains(3)); // type=Note
    }

    #[test]
    fn test_evaluate_must_with_missing_child_is_empty() {
        let meta = setup_test_index();
        let filter = JsonLdFilter::Must(vec![
            JsonLdFilter::tag("tags", "important"),
            JsonLdFilter::tag("tags", "absent"),
        ]);
        let bm = compile_filter(&filter).evaluate(&meta).unwrap();
        assert!(bm.is_empty());
    }

    #[test]
    fn test_evaluate_empty_must_matches_all_ids() {
        let meta = setup_test_index();
        let bm = compile_filter(&JsonLdFilter::Must(vec![]))
            .evaluate(&meta)
            .unwrap();
        let all_ids: RoaringBitmap = meta.all_ids().iter().collect();
        assert_eq!(bm, all_ids);
    }

    #[test]
    fn test_evaluate_should() {
        let meta = setup_test_index();
        let filter = JsonLdFilter::Should(vec![
            JsonLdFilter::Type("Note".into()),
            JsonLdFilter::tag("tags", "normal"),
        ]);
        let cf = compile_filter(&filter);
        let bm = cf.evaluate(&meta).unwrap();
        assert!(!bm.contains(1));
        assert!(bm.contains(2));
        assert!(bm.contains(3));
    }

    #[test]
    fn test_non_matching_compiled_filters_return_empty_bitmaps() {
        let meta = setup_test_index();
        let should = JsonLdFilter::Should(vec![JsonLdFilter::tag("tags", "absent")]);
        let must_not = JsonLdFilter::MustNot(vec![JsonLdFilter::Should(vec![
            JsonLdFilter::Type("Document".into()),
            JsonLdFilter::Type("Note".into()),
        ])]);

        assert!(compile_filter(&should).evaluate(&meta).unwrap().is_empty());
        assert!(compile_filter(&must_not)
            .evaluate(&meta)
            .unwrap()
            .is_empty());
        assert!(evaluate_filters(&meta, &[]).is_none());
    }

    #[test]
    fn test_evaluate_must_not() {
        let meta = setup_test_index();
        let filter = JsonLdFilter::MustNot(vec![JsonLdFilter::tag("tags", "important")]);
        let cf = compile_filter(&filter);
        let bm = cf.evaluate(&meta).unwrap();
        assert!(!bm.contains(1));
        assert!(bm.contains(2));
        assert!(!bm.contains(3));
    }
}
