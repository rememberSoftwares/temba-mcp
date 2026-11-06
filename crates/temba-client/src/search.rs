//! Small local BM25 index; no embedding service or remote search requests.
use std::collections::HashMap;

pub fn tool() -> rmcp::model::Tool {
    rmcp::model::Tool::new(
        "tool_search",
        "List MCPs with query=\"\" (no tool schemas). Use keywords for detailed tool search. Empty or omitted query returns a compact server inventory. Keyword results are capped per catalog; check total_matches and truncated. Pass local_ids only for private dynamic registrations.",
        serde_json::json!({
            "type":"object",
            "properties":{
                "query":{
                    "type":"string", "default":"",
                    "description":"Empty or omitted: list visible MCP servers and states, without tool descriptions or input schemas. Keywords: find detailed tools, capped by search_limit per catalog; inspect total_matches and truncated."
                },
                "local_ids":{
                    "type":"array", "items":{"type":"string"}, "maxItems":64,
                    "description":"Secret IDs of dynamic registrations to inspect/search exclusively. Omit to use configured catalogs."
                }
            },
            "additionalProperties":false
        }).as_object().unwrap().clone()
    )
}
pub fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}
pub fn scores(query: &str, documents: &[String]) -> Vec<f64> {
    let q = words(query);
    let docs: Vec<_> = documents.iter().map(|s| words(s)).collect();
    let average =
        (docs.iter().map(Vec::len).sum::<usize>() as f64 / docs.len().max(1) as f64).max(1.);
    let mut df = HashMap::new();
    for word in &q {
        df.entry(word)
            .or_insert_with(|| docs.iter().filter(|d| d.contains(word)).count() as f64);
    }
    docs.iter()
        .map(|d| {
            q.iter()
                .map(|w| {
                    let n = *df.get(w).unwrap();
                    let tf = d.iter().filter(|v| *v == w).count() as f64;
                    let idf = (1. + (docs.len() as f64 - n + 0.5) / (n + 0.5)).ln();
                    idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * d.len() as f64 / average))
                })
                .sum()
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranks_relevant_tools() {
        let s = scores(
            "jira tickets",
            &[
                "jira search tickets issues".into(),
                "grafana dashboards".into(),
            ],
        );
        assert!(s[0] > s[1]);
        assert_eq!(s[1], 0.);
    }
}
