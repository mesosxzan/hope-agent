use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, REFERER};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

use super::helpers::{
    html_decode, read_text_capped, strip_html_tags, DEFAULT_WEB_FETCH_USER_AGENT,
    HTML_RESPONSE_BYTE_CAP, JSON_RESPONSE_BYTE_CAP,
};
use super::{SearchParams, SearchResult};

/// Timestamp (epoch secs) until which DDG is rate-limited. Skip requests until then.
static DDG_RATE_LIMITED_UNTIL: AtomicU64 = AtomicU64::new(0);
/// Cooldown period after DDG rate-limits us (seconds).
const DDG_RATE_LIMIT_COOLDOWN_SECS: u64 = 30;

fn ddg_is_rate_limited() -> bool {
    let until = DDG_RATE_LIMITED_UNTIL.load(Ordering::Relaxed);
    if until == 0 {
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now < until
}

fn ddg_mark_rate_limited() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    DDG_RATE_LIMITED_UNTIL.store(now + DDG_RATE_LIMIT_COOLDOWN_SECS, Ordering::Relaxed);
}

/// Map freshness filter to DDG's `df` parameter value.
/// DDG time filters: d=day, w=week, m=month
fn ddg_freshness_param(f: Option<&str>) -> &'static str {
    match f {
        Some("day") => "d",
        Some("week") => "w",
        Some("month") => "m",
        _ => "",
    }
}

pub(super) async fn search_duckduckgo(
    query: &str,
    count: usize,
    timeout_secs: u64,
    params: &SearchParams,
) -> Result<Vec<SearchResult>> {
    // Skip if recently rate-limited
    if ddg_is_rate_limited() {
        app_warn!(
            "tool",
            "web_search",
            "DDG rate-limit cooldown active, skipping"
        );
        // Fall through to Instant Answer API instead of failing entirely
        let client = build_ddg_client(timeout_secs)?;
        let instant_results = ddg_instant_answer(&client, query).await;
        if !instant_results.is_empty() {
            return Ok(instant_results);
        }
        return Err(anyhow::anyhow!("DuckDuckGo rate-limit cooldown active"));
    }

    let client = build_ddg_client(timeout_secs)?;

    // 1. Primary: HTML search (GET — POST triggers DDG's anti-bot challenge)
    let mut results = ddg_html_search(&client, query, count, params.freshness.as_deref()).await;

    // 2. If HTML search failed or returned few results, supplement with Instant Answer API
    let html_ok = results.is_ok();
    let html_count = results.as_ref().map(|r| r.len()).unwrap_or(0);

    if !html_ok || html_count < 3 {
        if !html_ok {
            app_info!(
                "tool",
                "web_search",
                "DDG HTML search failed, falling back to Instant Answer API"
            );
        } else {
            app_info!(
                "tool",
                "web_search",
                "DDG HTML search returned {} results (<3), supplementing with Instant API",
                html_count
            );
        }
        let instant_results = ddg_instant_answer(&client, query).await;

        // Merge: prefer HTML results, add Instant results not already present
        let html_results = results.unwrap_or_default();
        let existing_urls: std::collections::HashSet<String> =
            html_results.iter().map(|r| r.url.clone()).collect();
        let mut merged = html_results;
        for ir in instant_results {
            if !existing_urls.contains(&ir.url) {
                merged.push(ir);
            }
        }
        results = Ok(merged);
    }

    // Deduplicate by URL
    let mut seen = std::collections::HashSet::new();
    let mut final_results = results?;
    final_results.retain(|r| {
        if r.url.is_empty() {
            return true;
        }
        seen.insert(r.url.clone())
    });

    final_results.truncate(count);
    Ok(final_results)
}

/// Build a client with browser-like headers and web-search-specific proxy.
fn build_ddg_client(timeout_secs: u64) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    // Simulate a real browser request to avoid bot detection
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7"),
    );
    headers.insert(REFERER, HeaderValue::from_static("https://duckduckgo.com/"));
    headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("document"));
    headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("navigate"));
    headers.insert("Sec-Fetch-Site", HeaderValue::from_static("same-origin"));
    headers.insert("Sec-Fetch-User", HeaderValue::from_static("?1"));

    // Use web-search-specific proxy (with fallback to global proxy)
    let proxy_config = super::effective_web_search_proxy();
    crate::provider::apply_proxy_from_config(
        reqwest::Client::builder()
            .user_agent(DEFAULT_WEB_FETCH_USER_AGENT)
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(timeout_secs)),
        &proxy_config,
    )
    .build()
    .map_err(|e| anyhow::anyhow!("Failed to create DDG HTTP client: {}", e))
}

/// Primary DDG search via the HTML endpoint (GET with query params).
/// POST triggers DDG's anti-bot challenge (captcha), so GET is required.
/// Supports freshness filter via the `df` parameter.
async fn ddg_html_search(
    client: &reqwest::Client,
    query: &str,
    count: usize,
    freshness: Option<&str>,
) -> Result<Vec<SearchResult>> {
    let df = ddg_freshness_param(freshness);
    let mut url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );
    if !df.is_empty() {
        url.push_str(&format!("&df={}", df));
    }

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("DuckDuckGo HTML request failed: {}", e))?;

    let status = resp.status();
    // DDG returns 202 when rate-limited
    if status == reqwest::StatusCode::ACCEPTED {
        app_warn!(
            "tool",
            "web_search",
            "DDG rate-limited (HTTP 202), cooldown {}s",
            DDG_RATE_LIMIT_COOLDOWN_SECS
        );
        ddg_mark_rate_limited();
        return Err(anyhow::anyhow!("DDG rate-limited (HTTP 202)"));
    }
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "DuckDuckGo HTML failed with status: {}",
            status
        ));
    }
    let html = read_text_capped(resp, HTML_RESPONSE_BYTE_CAP)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read DuckDuckGo response: {}", e))?;

    // Detect anti-bot redirect: DDG returns homepage instead of results
    if is_ddg_blocked(&html) {
        app_warn!(
            "tool",
            "web_search",
            "DDG HTML returned homepage (anti-bot/rate-limit), {}B response, cooldown {}s",
            html.len(),
            DDG_RATE_LIMIT_COOLDOWN_SECS
        );
        ddg_mark_rate_limited();
        return Err(anyhow::anyhow!("DDG blocked (anti-bot redirect)"));
    }

    let results = parse_ddg_results(&html, count);
    if results.is_empty() {
        let preview = crate::truncate_utf8(&html, 2048);
        app_warn!(
            "tool",
            "web_search",
            "DDG HTML parsed 0 results, raw response ({}B, preview {}B):\n{}",
            html.len(),
            preview.len(),
            preview
        );
    }
    Ok(results)
}

/// DuckDuckGo Instant Answer API — returns structured data for factual queries.
/// Enriched with RelatedTopics and Results for broader coverage.
async fn ddg_instant_answer(client: &reqwest::Client, query: &str) -> Vec<SearchResult> {
    let url = format!(
        "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
        urlencoding::encode(query)
    );
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let text = match read_text_capped(resp, JSON_RESPONSE_BYTE_CAP).await {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let data: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();

    // AbstractText + AbstractURL — encyclopedia-style answer
    let abstract_text = data
        .get("AbstractText")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let abstract_url = data
        .get("AbstractURL")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let heading = data.get("Heading").and_then(|v| v.as_str()).unwrap_or("");

    if !abstract_text.is_empty() && !abstract_url.is_empty() {
        results.push(SearchResult {
            title: if heading.is_empty() {
                "Instant Answer".into()
            } else {
                heading.to_string()
            },
            url: abstract_url.to_string(),
            snippet: abstract_text.chars().take(300).collect(),
            source: "DuckDuckGo".into(),
        });
    }

    // Answer field — direct factual answer (e.g. calculations, conversions)
    let answer = data.get("Answer").and_then(|v| v.as_str()).unwrap_or("");
    if !answer.is_empty() {
        results.push(SearchResult {
            title: format!("{} — Instant Answer", query),
            url: String::new(),
            snippet: answer.to_string(),
            source: "DuckDuckGo".into(),
        });
    }

    // Related topics — additional context links
    if let Some(topics) = data.get("RelatedTopics").and_then(|v| v.as_array()) {
        for topic in topics.iter().take(5) {
            let text = topic.get("Text").and_then(|v| v.as_str()).unwrap_or("");
            let first_url = topic.get("FirstURL").and_then(|v| v.as_str()).unwrap_or("");
            if !text.is_empty() && !first_url.is_empty() {
                let title = text.split(" - ").next().unwrap_or("Related Topic");
                results.push(SearchResult {
                    title: title.to_string(),
                    url: first_url.to_string(),
                    snippet: text.to_string(),
                    source: "DuckDuckGo".into(),
                });
            }
        }
    }

    // Results — direct result links
    if let Some(direct_results) = data.get("Results").and_then(|v| v.as_array()) {
        for result in direct_results.iter().take(3) {
            let text = result.get("Text").and_then(|v| v.as_str()).unwrap_or("");
            let first_url = result
                .get("FirstURL")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !text.is_empty() && !first_url.is_empty() {
                results.push(SearchResult {
                    title: text.to_string(),
                    url: first_url.to_string(),
                    snippet: String::new(),
                    source: "DuckDuckGo".into(),
                });
            }
        }
    }

    results
}

fn parse_ddg_results(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut pos = 0;

    while results.len() < max_results {
        let link_marker = "class=\"result__a\"";
        let link_start = match html[pos..].find(link_marker) {
            Some(idx) => pos + idx,
            None => break,
        };

        // Find the enclosing <a tag start — search backward from class marker
        // to find "<a " which opens this tag.
        let tag_open = match html[..link_start].rfind("<a ") {
            Some(idx) => idx,
            None => {
                pos = link_start + link_marker.len();
                continue;
            }
        };

        // Find the closing ">" of the <a> opening tag
        let tag_close = match html[tag_open..].find('>') {
            Some(idx) => tag_open + idx,
            None => {
                pos = link_start + link_marker.len();
                continue;
            }
        };

        // Now search for href="..." within the <a ...> opening tag only
        let tag_content = &html[tag_open..tag_close];
        let href_in_tag = match tag_content.find("href=\"") {
            Some(idx) => tag_open + idx + 6,
            None => {
                pos = link_start + link_marker.len();
                continue;
            }
        };
        let href_end = match html[href_in_tag..].find('"') {
            Some(idx) => href_in_tag + idx,
            None => {
                pos = link_start + link_marker.len();
                continue;
            }
        };
        let raw_url = &html[href_in_tag..href_end];
        let url = extract_ddg_url(raw_url);

        let title_start = match html[link_start..].find('>') {
            Some(idx) => link_start + idx + 1,
            None => {
                pos = link_start + link_marker.len();
                continue;
            }
        };
        let title_end = match html[title_start..].find("</a>") {
            Some(idx) => title_start + idx,
            None => {
                pos = link_start + link_marker.len();
                continue;
            }
        };
        let title = strip_html_tags(&html[title_start..title_end]);

        let snippet_marker = "class=\"result__snippet\"";
        let snippet = if let Some(snippet_start) = html[title_end..].find(snippet_marker) {
            let abs_snippet_start = title_end + snippet_start;
            if let Some(tag_end) = html[abs_snippet_start..].find('>') {
                let content_start = abs_snippet_start + tag_end + 1;
                // Try multiple end markers — DDG wraps snippets in <a> or <span>
                let end_pos = [
                    html[content_start..].find("</a>"),
                    html[content_start..].find("</span>"),
                    html[content_start..].find("</div>"),
                ]
                .iter()
                .filter_map(|x| *x)
                .min()
                .unwrap_or(0);
                if end_pos > 0 {
                    strip_html_tags(&html[content_start..content_start + end_pos])
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // Extract display URL from result__url class
        let url_marker = "class=\"result__url\"";
        let display_url = if let Some(um_start) = html[title_end..].find(url_marker) {
            let abs_um = title_end + um_start;
            if let Some(tag_end) = html[abs_um..].find('>') {
                let content_start = abs_um + tag_end + 1;
                if let Some(end) = html[content_start..].find('<') {
                    html[content_start..content_start + end].trim().to_string()
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        if !title.is_empty() && !url.is_empty() && !url.starts_with("javascript:") {
            results.push(SearchResult {
                title: html_decode(&title),
                url,
                snippet: if snippet.is_empty() {
                    display_url
                } else {
                    html_decode(&snippet)
                },
                source: "DuckDuckGo".into(),
            });
        }

        pos = title_end;
    }

    results
}

/// Detect if DDG returned its homepage instead of search results (anti-bot block).
fn is_ddg_blocked(html: &str) -> bool {
    // When blocked, DDG returns a page with canonical URL pointing to the homepage
    // and no search result markers at all
    let has_canonical_home = html.contains(r#"rel="canonical" href="https://duckduckgo.com/"#);
    let has_no_results = !html.contains("result__a")
        && !html.contains("result-link")
        && !html.contains("result__snippet");
    has_canonical_home && has_no_results
}

fn extract_ddg_url(raw: &str) -> String {
    if let Some(uddg_start) = raw.find("uddg=") {
        let url_start = uddg_start + 5;
        let url_end = raw[url_start..]
            .find('&')
            .map(|i| url_start + i)
            .unwrap_or(raw.len());
        let encoded = &raw[url_start..url_end];
        urlencoding::decode(encoded)
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| encoded.to_string())
    } else if raw.starts_with("http") {
        raw.to_string()
    } else {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SOCKS5 proxy for testing (user-provided)
    /// Use socks5h:// for remote DNS resolution (required when local DNS can't
    /// resolve target domains or local network can't reach them directly)
    const TEST_SOCKS5_PROXY: &str = "socks5h://192.168.1.97:11281";

    fn build_test_client_with_proxy() -> reqwest::Client {
        let proxy = reqwest::Proxy::all(TEST_SOCKS5_PROXY)
            .expect("Failed to create SOCKS5 proxy");
        reqwest::Client::builder()
            .user_agent(DEFAULT_WEB_FETCH_USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .proxy(proxy)
            .build()
            .expect("Failed to build test client")
    }

    /// Diagnostic test: try HTML GET search and verify parsing works
    /// Run with: cargo test -p ha-core ddg_html_search_diagnostic -- --nocapture
    #[tokio::test]
    async fn ddg_html_search_diagnostic() {
        let client = build_test_client_with_proxy();

        println!("=== GET HTML search ===");
        let url = "https://html.duckduckgo.com/html/?q=rust+programming";
        let resp = client.get(url).send().await;

        match resp {
            Ok(r) => {
                println!("Status: {}", r.status());
                let html = read_text_capped(r, HTML_RESPONSE_BYTE_CAP).await.unwrap();
                println!("Body length: {} bytes", html.len());
                println!("  result__a: {}", html.contains("result__a"));
                println!("  is_blocked: {}", is_ddg_blocked(&html));

                let results = parse_ddg_results(&html, 10);
                println!("  Parsed {} results", results.len());
                for (i, r) in results.iter().enumerate() {
                    println!("    {}. {} -> {}", i + 1, r.title, r.url);
                }

                assert!(!results.is_empty(), "Should parse at least 1 result from DDG HTML search");
            }
            Err(e) => {
                println!("GET request FAILED: {}", e);
                panic!("DDG HTML search failed — check proxy connectivity");
            }
        }
    }

    /// Diagnostic test: try Instant Answer API
    /// Run with: cargo test -p ha-core ddg_instant_answer_diagnostic -- --nocapture
    #[tokio::test]
    async fn ddg_instant_answer_diagnostic() {
        let client = build_test_client_with_proxy();

        println!("=== Instant Answer API ===");
        let results = ddg_instant_answer(&client, "rust programming language").await;
        println!("Got {} instant answer results", results.len());
        for (i, r) in results.iter().enumerate() {
            println!("  {}. {} -> {}", i + 1, r.title, r.url);
        }
    }

    #[test]
    fn test_parse_ddg_results_with_sample() {
        // Old format: href before class (historical DDG HTML)
        let html_old = r#"
        <div class="result results_links results_links_deep web-result">
            <div class="links_main links_deep result__body">
                <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&rut=abc123" class="result__a">Rust Programming Language</a>
                <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">A language empowering everyone to build reliable software.</a>
                <a class="result__url" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">rust-lang.org</a>
            </div>
        </div>
        "#;

        let results_old = parse_ddg_results(html_old, 10);
        assert!(!results_old.is_empty(), "Should parse old-format results");
        assert_eq!(results_old[0].url, "https://www.rust-lang.org/");
        assert_eq!(results_old[0].title, "Rust Programming Language");

        // New format: href after class (current DDG HTML)
        let html_new = r#"
        <h2 class="result__title">
            <a rel="nofollow" class="result__a" href="https://rust-lang.org/">Rust Programming Language</a>
        </h2>
        <h2 class="result__title">
            <a rel="nofollow" class="result__a" href="https://en.wikipedia.org/wiki/Rust_(programming_language)">Rust (programming language) - Wikipedia</a>
        </h2>
        <h2 class="result__title">
            <a rel="nofollow" class="result__a" href="https://www.w3schools.com/rust/index.php">Rust Tutorial - W3Schools</a>
        </h2>
        "#;

        let results_new = parse_ddg_results(html_new, 10);
        assert!(!results_new.is_empty(), "Should parse new-format results");
        assert_eq!(results_new.len(), 3);
        assert_eq!(results_new[0].url, "https://rust-lang.org/");
        assert_eq!(results_new[0].title, "Rust Programming Language");
        assert_eq!(results_new[1].url, "https://en.wikipedia.org/wiki/Rust_(programming_language)");
        assert_eq!(results_new[2].url, "https://www.w3schools.com/rust/index.php");

        // Mixed format in same page
        let html_mixed = r#"
        <a href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F1" class="result__a">Old Format Link</a>
        <a class="result__a" href="https://example.com/2">New Format Link</a>
        "#;

        let results_mixed = parse_ddg_results(html_mixed, 10);
        assert_eq!(results_mixed.len(), 2, "Should parse both old and new format");
        assert_eq!(results_mixed[0].url, "https://example.com/1");
        assert_eq!(results_mixed[1].url, "https://example.com/2");
    }
}
