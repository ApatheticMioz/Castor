//! Academic paper lookup client querying OpenAlex (`api.openalex.org`).
//!
//! Provides deterministic bibliographic metadata (title, year, venue, DOI,
//! author institutional affiliations, and direct open-access PDF URLs) to prevent
//! LLM hallucination during academic and literature research.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

const DEFAULT_TIMEOUT_SECS: u64 = 15;
const DEFAULT_USER_AGENT: &str = "Castor/1.1 (mailto:castor@local.dev)";

/// Errors produced by [`PaperClient`].
#[derive(Debug, Error)]
pub enum PaperError {
    #[error("InvalidQuery: search query cannot be empty")]
    InvalidQuery,

    #[error("HttpError: {method} '{url}' failed (HTTP {status}): {reason}")]
    Http {
        method: String,
        url: String,
        status: u16,
        reason: String,
    },

    #[error("JsonError: failed to parse OpenAlex response from '{url}': {reason}")]
    Json { url: String, reason: String },
}

/// A structured academic author with institutional affiliations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaperAuthor {
    pub name: String,
    pub affiliations: Vec<String>,
}

/// A verified academic paper entity from OpenAlex.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaperEntity {
    pub id: String,
    pub doi: Option<String>,
    pub title: String,
    pub publication_year: Option<u32>,
    pub venue: Option<String>,
    pub authors: Vec<PaperAuthor>,
    pub open_access_pdf: Option<String>,
    pub landing_page_url: Option<String>,
    pub abstract_text: Option<String>,
    pub cited_by_count: Option<u64>,
}

impl PaperEntity {
    /// Format a clean, structured text representation for model consumption.
    pub fn format_display(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("**Title**: {}\n", self.title));

        if let Some(year) = self.publication_year {
            out.push_str(&format!("**Year**: {year}\n"));
        }
        if let Some(ref venue) = self.venue {
            out.push_str(&format!("**Venue**: {venue}\n"));
        }
        if let Some(ref doi) = self.doi {
            out.push_str(&format!("**DOI**: {doi}\n"));
        }
        if let Some(ref pdf) = self.open_access_pdf {
            out.push_str(&format!("**Open Access PDF**: {pdf}\n"));
        } else if let Some(ref landing) = self.landing_page_url {
            out.push_str(&format!("**Landing Page**: {landing}\n"));
        }

        if !self.authors.is_empty() {
            let author_strs: Vec<String> = self
                .authors
                .iter()
                .map(|a| {
                    if a.affiliations.is_empty() {
                        a.name.clone()
                    } else {
                        format!("{} ({})", a.name, a.affiliations.join(", "))
                    }
                })
                .collect();
            out.push_str(&format!("**Authors**: {}\n", author_strs.join("; ")));
        }

        if let Some(citations) = self.cited_by_count {
            out.push_str(&format!("**Citations**: {citations}\n"));
        }

        if let Some(ref abs) = self.abstract_text {
            out.push_str(&format!("\n**Abstract**:\n{abs}"));
        }

        out
    }
}

/// Client for academic paper lookup against OpenAlex.
#[derive(Debug, Clone)]
pub struct PaperClient {
    client: reqwest::Client,
    base_url: String,
}

impl PaperClient {
    pub fn new() -> Self {
        Self::with_base_url("https://api.openalex.org")
    }

    pub fn with_base_url(base_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .user_agent(DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Lookup papers by title, keywords, DOI, or OpenAlex ID.
    pub async fn lookup(&self, query: &str, limit: usize) -> Result<Vec<PaperEntity>, PaperError> {
        let q = query.trim();
        if q.is_empty() {
            return Err(PaperError::InvalidQuery);
        }

        let per_page = limit.clamp(1, 10);

        // Check if query is a DOI or OpenAlex Work ID (e.g. "10.1109/..." or "https://doi.org/..." or "W...")
        let target_url = if q.starts_with("10.") || q.contains("doi.org/10.") {
            let clean_doi = if let Some(idx) = q.find("10.") {
                &q[idx..]
            } else {
                q
            };
            format!("{}/works/https://doi.org/{}", self.base_url, clean_doi)
        } else if q.starts_with('W') && q.chars().skip(1).all(|c| c.is_ascii_digit()) {
            format!("{}/works/{}", self.base_url, q)
        } else {
            let mut url =
                reqwest::Url::parse(&format!("{}/works", self.base_url)).map_err(|e| {
                    PaperError::Http {
                        method: "GET".into(),
                        url: self.base_url.clone(),
                        status: 0,
                        reason: format!("invalid base url: {e}"),
                    }
                })?;
            url.query_pairs_mut()
                .append_pair("search", q)
                .append_pair("per-page", &per_page.to_string());
            url.to_string()
        };

        let resp = self
            .client
            .get(&target_url)
            .send()
            .await
            .map_err(|e| PaperError::Http {
                method: "GET".into(),
                url: target_url.clone(),
                status: 0,
                reason: e.to_string(),
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(PaperError::Http {
                method: "GET".into(),
                url: target_url,
                status: status.as_u16(),
                reason: status.canonical_reason().unwrap_or("").to_string(),
            });
        }

        let json_body: serde_json::Value = resp.json().await.map_err(|e| PaperError::Json {
            url: target_url.clone(),
            reason: e.to_string(),
        })?;

        // If queried direct entity (by DOI or ID), parse single object. Otherwise parse results array.
        if let Some(results) = json_body.get("results").and_then(|r| r.as_array()) {
            let entities = results.iter().filter_map(parse_openalex_work).collect();
            Ok(entities)
        } else if json_body.get("id").is_some() {
            if let Some(entity) = parse_openalex_work(&json_body) {
                Ok(vec![entity])
            } else {
                Ok(vec![])
            }
        } else {
            Ok(vec![])
        }
    }
}

impl Default for PaperClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Reconstruct full text from OpenAlex's abstract_inverted_index.
fn reconstruct_abstract(inverted: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let mut words: Vec<(usize, &str)> = Vec::new();
    for (word, positions) in inverted {
        if let Some(pos_arr) = positions.as_array() {
            for pos in pos_arr {
                if let Some(idx) = pos.as_u64() {
                    words.push((idx as usize, word.as_str()));
                }
            }
        }
    }
    if words.is_empty() {
        return None;
    }
    words.sort_by_key(|&(idx, _)| idx);
    Some(words.iter().map(|&(_, w)| w).collect::<Vec<_>>().join(" "))
}

/// Parse a single OpenAlex work object into a [`PaperEntity`].
fn parse_openalex_work(val: &serde_json::Value) -> Option<PaperEntity> {
    let obj = val.as_object()?;

    let id = obj.get("id")?.as_str()?.to_string();
    let title = obj
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("[Untitled]")
        .to_string();
    let publication_year = obj
        .get("publication_year")
        .and_then(|v| v.as_u64())
        .map(|y| y as u32);

    let doi = obj
        .get("doi")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let venue = obj
        .get("primary_location")
        .and_then(|loc| loc.get("source"))
        .and_then(|src| src.get("display_name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let open_access_pdf = obj
        .get("best_oa_location")
        .or_else(|| obj.get("primary_location"))
        .and_then(|loc| loc.get("pdf_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let landing_page_url = obj
        .get("primary_location")
        .and_then(|loc| loc.get("landing_page_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let cited_by_count = obj.get("cited_by_count").and_then(|v| v.as_u64());

    let abstract_text = obj
        .get("abstract_inverted_index")
        .and_then(|v| v.as_object())
        .and_then(reconstruct_abstract);

    let mut authors = Vec::new();
    if let Some(authorships) = obj.get("authorships").and_then(|v| v.as_array()) {
        for a in authorships {
            let name = a
                .get("author")
                .and_then(|author| author.get("display_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if name.is_empty() {
                continue;
            }

            let mut affiliations = Vec::new();
            if let Some(insts) = a.get("institutions").and_then(|v| v.as_array()) {
                for inst in insts {
                    if let Some(inst_name) = inst.get("display_name").and_then(|v| v.as_str()) {
                        affiliations.push(inst_name.to_string());
                    }
                }
            }

            authors.push(PaperAuthor { name, affiliations });
        }
    }

    Some(PaperEntity {
        id,
        doi,
        title,
        publication_year,
        venue,
        authors,
        open_access_pdf,
        landing_page_url,
        abstract_text,
        cited_by_count,
    })
}

// ---------------------------------------------------------------------------
// Tests (axum mock server on ephemeral port, zero external network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    async fn start(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn json_body(s: &str) -> axum::response::Response {
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            s.to_string(),
        )
            .into_response()
    }

    async fn mock_openalex_works() -> axum::response::Response {
        json_body(
            r#"{
            "results": [
                {
                    "id": "https://openalex.org/W4386789123",
                    "doi": "https://doi.org/10.1109/ICC56840.2025.10992341",
                    "title": "RCACopilot: Automated Root Cause Analysis for Cloud Networks",
                    "publication_year": 2025,
                    "primary_location": {
                        "source": { "display_name": "IEEE International Conference on Communications" },
                        "landing_page_url": "https://doi.org/10.1109/ICC56840.2025.10992341",
                        "pdf_url": "https://arxiv.org/pdf/2507.03224.pdf"
                    },
                    "best_oa_location": {
                        "pdf_url": "https://arxiv.org/pdf/2507.03224.pdf"
                    },
                    "authorships": [
                        {
                            "author": { "display_name": "Alexander Shan" },
                            "institutions": [ { "display_name": "Stanford University" } ]
                        },
                        {
                            "author": { "display_name": "Raj Yavatkar" },
                            "institutions": [ { "display_name": "Juniper Networks" } ]
                        }
                    ],
                    "abstract_inverted_index": {
                        "Automated": [0],
                        "root": [1],
                        "cause": [2],
                        "analysis": [3]
                    },
                    "cited_by_count": 4
                }
            ]
        }"#,
        )
    }

    async fn mock_openalex_doi() -> axum::response::Response {
        json_body(
            r#"{
            "id": "https://openalex.org/W12345",
            "doi": "https://doi.org/10.1145/2785956.2787498",
            "title": "Batfish: A Tool for Testing and Verifying Network Configurations",
            "publication_year": 2015,
            "primary_location": {
                "source": { "display_name": "USENIX NSDI" },
                "landing_page_url": "https://www.usenix.org/conference/nsdi15/technical-sessions/presentation/fogel",
                "pdf_url": "https://www.usenix.org/system/files/conference/nsdi15/nsdi15-paper-fogel.pdf"
            },
            "authorships": [
                {
                    "author": { "display_name": "Ari Fogel" },
                    "institutions": [ { "display_name": "UCLA" } ]
                },
                {
                    "author": { "display_name": "Rataprathap" },
                    "institutions": [ { "display_name": "Microsoft" } ]
                }
            ],
            "abstract_inverted_index": {
                "Batfish": [0],
                "validates": [1],
                "configurations": [2]
            },
            "cited_by_count": 250
        }"#,
        )
    }

    fn router() -> Router {
        Router::new()
            .route("/works", get(mock_openalex_works))
            .route(
                "/works/https://doi.org/10.1145/2785956.2787498",
                get(mock_openalex_doi),
            )
    }

    #[tokio::test]
    async fn paper_lookup_search_query() {
        let base = start(router()).await;
        let c = PaperClient::with_base_url(&base);
        let papers = c.lookup("RCACopilot", 3).await.unwrap();
        assert_eq!(papers.len(), 1);
        let p = &papers[0];
        assert_eq!(
            p.title,
            "RCACopilot: Automated Root Cause Analysis for Cloud Networks"
        );
        assert_eq!(p.publication_year, Some(2025));
        assert_eq!(p.authors.len(), 2);
        assert_eq!(p.authors[0].name, "Alexander Shan");
        assert_eq!(p.authors[0].affiliations, vec!["Stanford University"]);
        assert_eq!(p.authors[1].name, "Raj Yavatkar");
        assert_eq!(p.authors[1].affiliations, vec!["Juniper Networks"]);
        assert_eq!(
            p.open_access_pdf,
            Some("https://arxiv.org/pdf/2507.03224.pdf".to_string())
        );
        assert_eq!(
            p.abstract_text,
            Some("Automated root cause analysis".to_string())
        );
        assert!(p.format_display().contains("Juniper Networks"));
    }

    #[tokio::test]
    async fn paper_lookup_direct_doi() {
        let base = start(router()).await;
        let c = PaperClient::with_base_url(&base);
        let papers = c.lookup("10.1145/2785956.2787498", 1).await.unwrap();
        assert_eq!(papers.len(), 1);
        let p = &papers[0];
        assert_eq!(
            p.title,
            "Batfish: A Tool for Testing and Verifying Network Configurations"
        );
        assert_eq!(p.publication_year, Some(2015));
        assert_eq!(p.authors[0].name, "Ari Fogel");
        assert_eq!(p.authors[0].affiliations, vec!["UCLA"]);
    }

    #[tokio::test]
    async fn paper_lookup_empty_query() {
        let c = PaperClient::new();
        let err = c.lookup("   ", 1).await.unwrap_err();
        assert!(matches!(err, PaperError::InvalidQuery));
    }
}
