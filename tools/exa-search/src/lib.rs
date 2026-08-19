//! Search the web using Exa.ai and retrieve content from URLs
//!
//! A Genesis tool component. `describe` tells the model what this tool is and
//! what arguments it takes; `invoke` does the work. Edit this file with
//! `write_code` or `patch_code` — every edit rebuilds and reloads immediately,
//! and the compiler's output comes back in the tool result.

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
            name: "exa-search".to_string(),
            description: "Search the web using Exa.ai and retrieve content from URLs".to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["search", "contents"],
                        "description": "Action to perform: 'search' to search the web, 'contents' to get content from URLs"
                    },
                    "query": {
                        "type": "string",
                        "description": "Search query (required for 'search' action)"
                    },
                    "urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "List of URLs to get content from (required for 'contents' action)"
                    },
                    "num_results": {
                        "type": "integer",
                        "description": "Number of search results to return (default: 10)"
                    },
                    "highlights": {
                        "type": "boolean",
                        "description": "Include highlights in search results (default: true)"
                    },
                    "text": {
                        "type": "boolean",
                        "description": "Include full text content (default: true)"
                    }
                },
                "required": ["action"],
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

        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or("missing required argument 'action'")?;

        let api_key = config
            .get("api_key")
            .and_then(Value::as_str)
            .ok_or("missing 'api_key' in tool configuration")?;

        sys::log(LogLevel::Debug, &format!("exa-search action: {action}"));

        match action {
            "search" => handle_search(session_id, args, api_key),
            "contents" => handle_contents(session_id, args, api_key),
            _ => Err(format!("unknown action: {action}")),
        }
    }
}

fn handle_search(session_id: String, args: Value, api_key: &str) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or("missing required argument 'query' for search action")?;

    let num_results = args
        .get("num_results")
        .and_then(Value::as_i64)
        .unwrap_or(10);

    let highlights = args
        .get("highlights")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let text = args
        .get("text")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut request_body = json!({
        "query": query,
        "numResults": num_results,
    });

    let mut contents = json!({});
    if highlights {
        contents["highlights"] = json!(true);
    }
    if text {
        contents["text"] = json!(true);
    }
    if !contents.as_object().unwrap().is_empty() {
        request_body["contents"] = contents;
    }

    sys::log(LogLevel::Debug, &format!("Exa search request: {}", request_body));

    let request_json = request_body.to_string();
    
    // Build the curl command
    let curl_cmd = format!(
        "curl -X POST 'https://api.exa.ai/search' \
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

    // Parse and pretty-print the response
    match serde_json::from_str::<Value>(&result.stdout) {
        Ok(json) => Ok(serde_json::to_string_pretty(&json)
            .unwrap_or_else(|_| json.to_string())),
        Err(_) => Ok(result.stdout), // Return raw output if not JSON
    }
}

fn handle_contents(session_id: String, args: Value, api_key: &str) -> Result<String, String> {
    let urls = args
        .get("urls")
        .and_then(Value::as_array)
        .ok_or("missing required argument 'urls' for contents action")?;

    if urls.is_empty() {
        return Err("'urls' array cannot be empty".to_string());
    }

    let text = args
        .get("text")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let request_body = json!({
        "urls": urls,
        "text": text,
    });

    sys::log(LogLevel::Debug, &format!("Exa contents request: {}", request_body));

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

    // Parse and pretty-print the response
    match serde_json::from_str::<Value>(&result.stdout) {
        Ok(json) => Ok(serde_json::to_string_pretty(&json)
            .unwrap_or_else(|_| json.to_string())),
        Err(_) => Ok(result.stdout), // Return raw output if not JSON
    }
}

export!(Component);
