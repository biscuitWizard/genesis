//! Retrieve and parse web page contents using Exa.ai with full control over text extraction, highlights, and subpage inclusion
//!
//! Implements the Exa.ai /contents endpoint following their API reference and best practices.
//! Reference: https://exa.ai/docs/reference/get-contents

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

use genesis::harness::sandbox;
use genesis::harness::sys;
use genesis::harness::types::LogLevel;
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "web-contents".to_string(),
            description: "Retrieve and parse web page contents using Exa.ai with full control over text extraction, highlights, and subpage inclusion".to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "What to work on."
                    },
                    "ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "List of Exa document IDs to retrieve (from search results)"
                    },
                    "urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "List of URLs to retrieve content from"
                    },
                    "text": {
                        "type": "boolean",
                        "description": "Include cleaned and parsed text content. Default: false"
                    },
                    "highlights": {
                        "type": "boolean",
                        "description": "Include semantic highlights relevant to the query. Default: false"
                    },
                    "summary": {
                        "type": "boolean",
                        "description": "Include AI-generated summary. Default: false"
                    },
                    "livecrawl": {
                        "type": "string",
                        "enum": ["always", "fallback", "never"],
                        "description": "When to fetch fresh content. 'always': always crawl live, 'fallback': use cache or crawl if unavailable (default), 'never': only use cached content"
                    },
                    "subpages": {
                        "type": "integer",
                        "description": "Number of subpages to include (0-5). Requires autoprompt enabled. Default: 0"
                    },
                    "subpage_target": {
                        "type": "integer",
                        "description": "Minimum number of subpages to fetch before using summarization. Default: 3"
                    },
                    "max_characters": {
                        "type": "integer",
                        "description": "Maximum characters per page in text response. Min: 50, Max: 2500000"
                    }
                },
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["sandbox".to_string()],
        }
    }

    fn invoke(
        session_id: String,
        args_json: String,
        config_json: String,
    ) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("arguments were not valid JSON: {e}"))?;
        let config: Value = serde_json::from_str(&config_json).unwrap_or(json!({}));

        if !sandbox::available() {
            return Err("sandbox is not available, cannot make HTTP requests".to_string());
        }

        let api_key = config
            .get("api_key")
            .and_then(Value::as_str)
            .ok_or("missing 'api_key' in tool configuration - using parent exa-search key")?;

        // Build the request body according to Exa API spec
        let mut request_body = json!({});

        // Handle IDs or URLs (at least one required)
        let has_ids = args.get("ids").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false);
        let has_urls = args.get("urls").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false);

        if !has_ids && !has_urls {
            return Err("either 'ids' or 'urls' must be provided".to_string());
        }

        if has_ids {
            request_body["ids"] = args["ids"].clone();
        }
        if has_urls {
            request_body["urls"] = args["urls"].clone();
        }

        // Content options - following Exa best practices
        if let Some(text) = args.get("text").and_then(Value::as_bool) {
            if text {
                request_body["text"] = json!(true);
            }
        }

        if let Some(highlights) = args.get("highlights").and_then(Value::as_bool) {
            if highlights {
                request_body["highlights"] = json!(true);
            }
        }

        if let Some(summary) = args.get("summary").and_then(Value::as_bool) {
            if summary {
                request_body["summary"] = json!(true);
            }
        }

        // Livecrawl options
        if let Some(livecrawl) = args.get("livecrawl").and_then(Value::as_str) {
            match livecrawl {
                "always" | "fallback" | "never" => {
                    request_body["livecrawl"] = json!(livecrawl);
                }
                _ => return Err("livecrawl must be 'always', 'fallback', or 'never'".to_string()),
            }
        }

        // Subpages configuration
        if let Some(subpages) = args.get("subpages").and_then(Value::as_i64) {
            if subpages < 0 || subpages > 5 {
                return Err("subpages must be between 0 and 5".to_string());
            }
            if subpages > 0 {
                request_body["subpages"] = json!(subpages);
            }
        }

        if let Some(subpage_target) = args.get("subpage_target").and_then(Value::as_i64) {
            request_body["subpageTarget"] = json!(subpage_target);
        }

        // Text limits
        if let Some(max_chars) = args.get("max_characters").and_then(Value::as_i64) {
            if max_chars < 50 || max_chars > 2500000 {
                return Err("max_characters must be between 50 and 2500000".to_string());
            }
            request_body["maxCharacters"] = json!(max_chars);
        }

        sys::log(
            LogLevel::Debug,
            &format!("Exa contents request: {}", request_body),
        );

        let request_json = request_body.to_string();

        // Build the curl command
        let curl_cmd = format!(
            "curl -X POST 'https://api.exa.ai/contents' \
             -H 'x-api-key: {}' \
             -H 'Content-Type: application/json' \
             -d '{}' \
             2>&1",
            api_key,
            request_json.replace("'", "'\\''")
        );

        let result = sandbox::exec(&session_id, &curl_cmd, None, 30000);

        if result.exit_code != 0 {
            return Err(format!(
                "curl failed (exit {}): stdout: {} stderr: {}",
                result.exit_code, result.stdout, result.stderr
            ));
        }

        // Parse and format the response
        match serde_json::from_str::<Value>(&result.stdout) {
            Ok(json) => {
                // Check for API errors
                if let Some(error) = json.get("error") {
                    return Err(format!("Exa API error: {}", error));
                }

                Ok(serde_json::to_string_pretty(&json)
                    .unwrap_or_else(|_| json.to_string()))
            }
            Err(e) => {
                // If not valid JSON, check if it's an error message
                if result.stdout.contains("error") || result.stdout.contains("Error") {
                    Err(format!("API error: {}", result.stdout))
                } else {
                    Err(format!("Failed to parse response as JSON: {}. Raw output: {}", e, result.stdout))
                }
            }
        }
    }
}

export!(Component);
