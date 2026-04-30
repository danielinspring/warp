//! Tool schema definitions for advertising tools to LLM providers.

use serde::{Deserialize, Serialize};

/// A tool schema that can be advertised to an LLM provider.
///
/// Uses the OpenAI function-calling format since most providers
/// (Ollama, OpenAI, local servers) support it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    /// The tool name (used as the function name in calls).
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub parameters: serde_json::Value,
}

impl ToolSchema {
    /// Convert to the OpenAI function-calling tool format.
    pub fn to_openai_tool(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters
            }
        })
    }
}

/// Convenience builder for constructing tool schemas.
pub struct ToolSchemaBuilder {
    name: String,
    description: String,
    properties: serde_json::Map<String, serde_json::Value>,
    required: Vec<String>,
}

impl ToolSchemaBuilder {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            properties: serde_json::Map::new(),
            required: Vec::new(),
        }
    }

    /// Add a required string parameter.
    pub fn required_string(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        let name = name.into();
        self.properties.insert(
            name.clone(),
            serde_json::json!({
                "type": "string",
                "description": description.into()
            }),
        );
        self.required.push(name);
        self
    }

    /// Add an optional string parameter.
    pub fn optional_string(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.properties.insert(
            name.into(),
            serde_json::json!({
                "type": "string",
                "description": description.into()
            }),
        );
        self
    }

    /// Add a required boolean parameter.
    pub fn required_bool(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        let name = name.into();
        self.properties.insert(
            name.clone(),
            serde_json::json!({
                "type": "boolean",
                "description": description.into()
            }),
        );
        self.required.push(name);
        self
    }

    /// Build the final schema.
    pub fn build(self) -> ToolSchema {
        ToolSchema {
            name: self.name,
            description: self.description,
            parameters: serde_json::json!({
                "type": "object",
                "properties": self.properties,
                "required": self.required,
            }),
        }
    }
}
