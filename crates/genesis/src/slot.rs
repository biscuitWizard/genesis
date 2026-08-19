//! Slot identity.
//!
//! A "slot" is one hot-swappable position in the running system: the agent, a
//! gateway, or a tool. Slots are the unit of building, versioning, health
//! tracking, and rollback.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Slot {
    Agent,
    Gateway(String),
    Tool(String),
}

impl Slot {
    pub fn tool(name: impl Into<String>) -> Self {
        Slot::Tool(name.into())
    }

    pub fn gateway(name: impl Into<String>) -> Self {
        Slot::Gateway(name.into())
    }

    /// Stable key used in redb and in the `/admin` UI.
    pub fn key(&self) -> String {
        match self {
            Slot::Agent => "agent".to_string(),
            Slot::Gateway(n) => format!("gateway/{n}"),
            Slot::Tool(n) => format!("tool/{n}"),
        }
    }

    pub fn parse(key: &str) -> Result<Self> {
        match key.split_once('/') {
            None if key == "agent" => Ok(Slot::Agent),
            Some(("gateway", n)) if !n.is_empty() => Ok(Slot::Gateway(n.to_string())),
            Some(("tool", n)) if !n.is_empty() => Ok(Slot::Tool(n.to_string())),
            _ => Err(anyhow!("unknown slot key: {key}")),
        }
    }

    pub fn artifact_subdir(&self) -> String {
        match self {
            Slot::Agent => "agent".to_string(),
            Slot::Gateway(n) => format!("gateways/{n}"),
            Slot::Tool(n) => format!("tools/{n}"),
        }
    }

    /// Cargo package name of the source crate backing this slot.
    pub fn crate_name(&self) -> String {
        match self {
            Slot::Agent => "agent-core".to_string(),
            Slot::Gateway(n) => format!("gateway-{n}"),
            Slot::Tool(n) => format!("tool-{n}"),
        }
    }

    /// Filename cargo emits for the built component.
    pub fn wasm_filename(&self) -> String {
        format!("{}.wasm", self.crate_name().replace('-', "_"))
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

/// Names must be safe to use as a directory, a cargo package name, and a tool
/// name exposed to the model.
pub fn validate_component_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 48 {
        return Err(anyhow!("name must be 1-48 characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(anyhow!(
            "name must contain only lowercase letters, digits, and hyphens"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(anyhow!("name must not start or end with a hyphen"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_keys_round_trip() {
        for slot in [
            Slot::Agent,
            Slot::gateway("web"),
            Slot::tool("weather-lookup"),
        ] {
            assert_eq!(Slot::parse(&slot.key()).unwrap(), slot);
        }
    }

    #[test]
    fn rejects_unsafe_names() {
        for bad in ["", "Has-Upper", "has_underscore", "-lead", "trail-", "../x"] {
            assert!(validate_component_name(bad).is_err(), "accepted {bad:?}");
        }
        assert!(validate_component_name("weather-2").is_ok());
    }
}
