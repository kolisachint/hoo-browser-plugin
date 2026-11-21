//! `plugin/plugin.json` validation against hoocode's tier C native rules.
//!
//! Source of truth: hoocode's `crates/hoocode-code-plugins/src/native.rs`.
//! That check lives inside the hoocode binary, which exposes no manifest-
//! validation subcommand (`hoocode --version` here is 0.2.50 and offers no
//! such entry point), so invoking it is not feasible and this file
//! reimplements its checks instead (field names, platform keys, https URLs,
//! 64-hex sha256, tool name rules, inputSchema object with `type: object`,
//! permission hints) plus the identity between the manifest schemas and the
//! schemas served by `tools/list`. If hoocode ever gains a validation
//! command, prefer calling it over extending this reimplementation.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;

const PLATFORMS: &[&str] = &[
    "linux-x86_64",
    "linux-aarch64",
    "darwin-aarch64",
    "darwin-x86_64",
    "windows-x86_64",
];

fn plugin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugin")
}

fn read_json(name: &str) -> Value {
    let path = plugin_dir().join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} is not valid JSON: {e}"))
}

/// hoocode's `valid_plugin_tool_name`: bare names, never the `mcp__` prefix.
fn valid_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && !name.to_ascii_lowercase().starts_with("mcp__")
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[test]
fn manifest_head_matches_crate() {
    let plugin = read_json("plugin.json");
    assert_eq!(plugin["name"], Value::String("hoo-browser".into()));
    assert_eq!(
        plugin["version"],
        Value::String(env!("CARGO_PKG_VERSION").into())
    );
    for field in ["description", "license", "repository"] {
        assert!(
            plugin
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty()),
            "{field} must be a non-empty string"
        );
    }
}

#[test]
fn native_block_validates_like_hoocode_native_rs() {
    let plugin = read_json("plugin.json");
    let native = plugin
        .pointer("/extensions/dev.hoocode/native")
        .expect("extensions.dev.hoocode.native is required");
    assert_eq!(native["server"], Value::String("hoo-browser".into()));
    assert_eq!(native["shared"], Value::Bool(true));

    // binaries: known platform keys, https URLs, 64-hex sha256.
    let binaries = native["binaries"]
        .as_object()
        .expect("binaries must be an object");
    assert!(!binaries.is_empty(), "binaries must not be empty");
    for (platform, entry) in binaries {
        assert!(
            PLATFORMS.contains(&platform.as_str()),
            "unknown platform {platform:?}"
        );
        let url = entry["url"].as_str().unwrap_or("");
        assert!(
            url.starts_with("https://")
                && url.len() > "https://".len()
                && !url.chars().any(|c| c.is_whitespace() || c.is_control()),
            "binaries.{platform}.url must be an https URL"
        );
        let sha256 = entry["sha256"].as_str().unwrap_or("");
        assert!(
            sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "binaries.{platform}.sha256 must be 64 hex characters"
        );
        if sha256.chars().all(|c| c == '0') {
            eprintln!("note: binaries.{platform}.sha256 is the 64-zero placeholder (scripts/update_manifest.py fills it)");
        }
    }

    // tools: required, non-empty, unique (case-insensitive), valid names,
    // descriptions, object input schemas, read|act|dangerous permissions.
    let tools = native["tools"]
        .as_array()
        .expect("tools must be a non-empty array");
    assert!(!tools.is_empty());
    let mut seen = HashSet::new();
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("");
        assert!(valid_tool_name(name), "invalid tool name {name:?}");
        assert!(
            seen.insert(name.to_ascii_lowercase()),
            "duplicate tool {name:?}"
        );
        assert!(
            tool["description"].as_str().is_some_and(|s| !s.is_empty()),
            "tool {name:?} needs a description"
        );
        let schema = tool.get("inputSchema").unwrap_or(&Value::Null);
        assert!(
            schema
                .as_object()
                .is_some_and(|s| s.get("type") == Some(&Value::String("object".into()))),
            "tool {name:?} needs an inputSchema object with \"type\": \"object\""
        );
        let permission = tool
            .get("permission")
            .and_then(Value::as_str)
            .unwrap_or("act");
        assert!(
            ["read", "act", "dangerous"].contains(&permission),
            "tool {name:?} has bad permission {permission:?}"
        );
    }
}

#[test]
fn permission_hints_match_spec() {
    let plugin = read_json("plugin.json");
    let tools = plugin
        .pointer("/extensions/dev.hoocode/native/tools")
        .and_then(Value::as_array)
        .expect("tools array");
    let hint = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .and_then(|t| t.get("permission").and_then(Value::as_str))
            .unwrap_or("act")
    };
    // One hint per tool (spec B2): reads ask nothing, acts ask like edits,
    // eval is dangerous.
    for name in [
        "browser_open",
        "browser_snapshot",
        "browser_screenshot",
        "browser_wait",
    ] {
        assert_eq!(hint(name), "read", "{name} must be read");
    }
    for name in ["browser_act", "browser_tabs", "browser_nav"] {
        assert_eq!(hint(name), "act", "{name} must be act");
    }
    assert_eq!(hint("browser_eval"), "dangerous");
}

#[test]
fn manifest_schemas_match_served_schemas() {
    let plugin = read_json("plugin.json");
    let tools = plugin
        .pointer("/extensions/dev.hoocode/native/tools")
        .and_then(Value::as_array)
        .expect("tools array");
    let served = hoo_browser::server::tool_schemas();
    assert_eq!(
        tools.len(),
        served.len(),
        "manifest and tools/list disagree on tool count"
    );
    for (name, _description, schema) in &served {
        let manifest_tool = tools
            .iter()
            .find(|t| t["name"] == *name)
            .unwrap_or_else(|| panic!("served tool {name} missing from the manifest"));
        assert_eq!(
            &manifest_tool["inputSchema"], schema,
            "inputSchema for {name} differs between the manifest and tools/list"
        );
        assert_eq!(&manifest_tool["description"], _description);
    }
    // The manifest declares exactly the 8 tools the server lists.
    let declared: Vec<&str> = hoo_browser::server::DECLARED_TOOLS
        .iter()
        .map(|(n, _)| *n)
        .collect();
    assert_eq!(declared.len(), 8);
    assert_eq!(served.len(), 8);
}

#[test]
fn mcp_json_names_the_helper_server() {
    let mcp = read_json("mcp.json");
    assert_eq!(
        mcp["$schema"],
        Value::String("https://agent-plugins.org/schemas/1.0.0/mcp.schema.json".into())
    );
    let server = mcp
        .pointer("/mcpServers/hoo-browser")
        .expect("mcpServers.hoo-browser is required");
    assert_eq!(server["type"], Value::String("stdio".into()));
    assert_eq!(server["command"], Value::String("./bin/hoo-browser".into()));
    assert_eq!(
        server.pointer("/env/HOO_BROWSER_DATA"),
        Some(&Value::String("${PLUGIN_DATA}".into()))
    );
}

#[test]
fn skill_exists() {
    let path = plugin_dir().join("skills/browser/SKILL.md");
    let text = std::fs::read_to_string(&path).expect("skills/browser/SKILL.md is required");
    assert!(text.starts_with("---"), "SKILL.md needs frontmatter");
    assert!(
        text.contains("browser_snapshot"),
        "SKILL.md must teach the snapshot-before-act loop"
    );
    assert!(
        text.contains("hoo-browser"),
        "SKILL.md must document the CLI fallback"
    );
}
