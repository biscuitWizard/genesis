//! Deletes a tool from the system
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

// `tool-manifest` is already in scope from the world's own `use types.{...}`;
// anything else has to be imported from the types interface.
use genesis::harness::sys;
use genesis::harness::types::LogLevel;
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "delete-tool".to_string(),
            description: "Deletes a tool from the system".to_string(),
            // Must be a JSON Schema object: it becomes the tool's parameter
            // definition in the model's tool list.
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "What to work on."
                    }
                },
                "required": ["input"],
                "additionalProperties": false
            })
            .to_string(),
            // Host capabilities this tool needs, e.g. "sandbox".
            capabilities: vec![],
        }
    }

    fn invoke(_session_id: String, args_json: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("arguments were not valid JSON: {e}"))?;

        let tool_name = args
            .get("input")
            .and_then(Value::as_str)
            .ok_or("missing required argument 'input'")?;

        sys::log(LogLevel::Debug, &format!("delete-tool invoked with: {tool_name}"));

        // Attempt to delete the tool
        sys::delete_tool(tool_name)
            .map_err(|e| format!("failed to delete tool '{}': {}", tool_name, e))?;

        Ok(format!("Tool '{}' deleted successfully", tool_name))
    }
}

export!(Component);
