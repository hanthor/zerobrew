use serde::Serialize;
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cask {
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub url: String,
    pub homepage: Option<String>,
    pub binary: Option<String>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Artifact {
    Binary(String),
    App(String),
    Desktop(String),
    Icon(String),
    Unknown,
}

#[derive(Debug)]
pub enum CaskError {
    ParseError(String),
    MissingField(String),
}

impl std::fmt::Display for CaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaskError::ParseError(msg) => write!(f, "Failed to parse cask: {}", msg),
            CaskError::MissingField(field) => write!(f, "Missing required field: {}", field),
        }
    }
}

impl std::error::Error for CaskError {}

impl Cask {
    pub fn parse(content: &str, system_arch: &str) -> Result<Self, CaskError> {
        let mut parser = Parser::new();
        parser
            .set_language(tree_sitter_ruby::language())
            .expect("Error loading Ruby grammar");

        let tree = parser.parse(content, None).ok_or(CaskError::ParseError(
            "Failed to parse Ruby content".to_string(),
        ))?;

        let root_node = tree.root_node();
        let mut context = CaskContext {
            version: None,
            arch: system_arch.to_string(),
            variables: HashMap::new(),
            artifacts: Vec::new(),
        };

        // First pass: extract version to use in interpolation
        visit_node(root_node, content, &mut context, true)?;

        // Second pass: extract everything else
        visit_node(root_node, content, &mut context, false)?;

        let name = context
            .variables
            .get("name")
            .ok_or(CaskError::MissingField("name".to_string()))?
            .clone();
        let version = context
            .version
            .ok_or(CaskError::MissingField("version".to_string()))?;
        let sha256 = context
            .variables
            .get("sha256")
            .ok_or(CaskError::MissingField("sha256".to_string()))?
            .clone();
        let url = context
            .variables
            .get("url")
            .ok_or(CaskError::MissingField("url".to_string()))?
            .clone();

        let artifacts = context
            .artifacts
            .iter()
            .map(|(k, v)| match k.as_str() {
                "binary" => Artifact::Binary(v.clone()),
                "app" => Artifact::App(v.clone()),
                "artifact" => Artifact::Unknown, // Generic artifact
                _ => Artifact::Unknown,
            })
            .collect();

        // Extract binary if present (legacy single binary field)
        let binary = context.variables.get("binary").cloned();

        Ok(Cask {
            name,
            version,
            sha256,
            url,
            homepage: context.variables.get("homepage").cloned(),
            binary,
            artifacts,
        })
    }
}

struct CaskContext {
    version: Option<String>,
    arch: String, // "arm64_linux" or "x86_64_linux"
    variables: HashMap<String, String>,
    artifacts: Vec<(String, String)>,
}

fn visit_node(
    node: Node,
    source: &str,
    ctx: &mut CaskContext,
    version_pass: bool,
) -> Result<(), CaskError> {
    let kind = node.kind();
    // println!("Node kind: {}", kind); // Debugging

    if kind == "method_call" || kind == "command_call" || kind == "command" || kind == "call" {
        // Handle DSL methods: version, sha256, url, etc.
        if let Some(method_name_node) = node.child_by_field_name("method") {
            let method_name = method_name_node.utf8_text(source.as_bytes()).unwrap();

            match method_name {
                "version" => {
                    if version_pass && let Some(args) = node.child_by_field_name("arguments") {
                        // Extract raw string or handle comma-separated values
                        // For simplicity, just grab the first string literal or raw text
                        // version "1.2.3,12345" -> "1.2.3,12345"
                        let val = extract_string_arg(args, source, ctx)?;
                        ctx.version = Some(val);
                    }
                }
                "sha256" | "url" | "name" | "homepage" | "desc" => {
                    if !version_pass {
                        // Check if it has named arguments like `arm64_linux: "..."`
                        if let Some(args) = node.child_by_field_name("arguments") {
                            if let Some(val) = extract_arch_specific_arg(args, source, ctx) {
                                ctx.variables.insert(method_name.to_string(), val);
                            } else if let Ok(val) = extract_string_arg(args, source, ctx) {
                                // Fallback to positional arg if no arch match found (or if it's universal)
                                if !ctx.variables.contains_key(method_name) {
                                    ctx.variables.insert(method_name.to_string(), val);
                                }
                            }
                        }
                    }
                }
                "binary" | "artifact" | "app" => {
                    if !version_pass
                        && let Some(args) = node.child_by_field_name("arguments")
                        && let Ok(val) = extract_string_arg(args, source, ctx)
                    {
                        ctx.artifacts.push((method_name.to_string(), val));
                    }
                }
                _ => {}
            }
        }
    }

    // Recurse
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_node(child, source, ctx, version_pass)?;
    }

    Ok(())
}

fn extract_string_arg(
    args_node: Node,
    source: &str,
    ctx: &CaskContext,
) -> Result<String, CaskError> {
    // args_node is usually an 'argument_list' or similar
    // We want the first 'string' child
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "string" {
            let info = child
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim_matches('"')
                .to_string();
            // Interpolate
            return Ok(interpolate(&info, ctx));
        }
    }
    // Try bare word/number if strict string not found?
    Err(CaskError::ParseError(
        "Could not find string argument".to_string(),
    ))
}

fn extract_arch_specific_arg(args_node: Node, source: &str, ctx: &CaskContext) -> Option<String> {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        let kind = child.kind();
        // println!("DEBUG: child kind: {}", kind);
        if kind == "pair" {
            if let Some(val) = extract_pair_value(child, source, ctx) {
                return Some(val);
            }
        } else if kind == "hash" || kind == "bare_hash" {
            // println!("DEBUG: entering hash");
            // Recursively check inside hash
            if let Some(val) = extract_arch_specific_arg(child, source, ctx) {
                return Some(val);
            }
        }
    }
    None
}

fn extract_pair_value(pair_node: Node, source: &str, ctx: &CaskContext) -> Option<String> {
    if let Some(key) = pair_node.child_by_field_name("key") {
        let key_text = key.utf8_text(source.as_bytes()).unwrap_or("");
        // println!("DEBUG: key_text: '{}', ctx.arch: '{}'", key_text, ctx.arch);
        if key_text.trim_matches(':') == ctx.arch
            && let Some(val) = pair_node.child_by_field_name("value")
            && val.kind() == "string"
        {
            let s = val.utf8_text(source.as_bytes()).unwrap().trim_matches('"');
            return Some(interpolate(s, ctx));
        }
    }
    None
}

fn interpolate(s: &str, ctx: &CaskContext) -> String {
    let mut res = s.to_string();
    if let Some(ver) = &ctx.version {
        // handle #{version}
        res = res.replace("#{version}", ver);

        // handle #{version.csv.first} / #{version.csv.second}
        // "1.15.8,5724687216017408" -> first="1.15.8", second="5724687216017408"
        if ver.contains(',') {
            let parts: Vec<&str> = ver.split(',').collect();
            if !parts.is_empty() {
                res = res.replace("#{version.csv.first}", parts[0]);
            }
            if parts.len() > 1 {
                res = res.replace("#{version.csv.second}", parts[1]);
            }
        }
    }
    // handle #{arch} - somewhat ambiguous in casks, usually "arm" or "x64" for mac,
    // but antigravity-linux.rb uses "linux-#{arch}"
    // We can infer simplified arch from our system arch string
    let simple_arch = if ctx.arch.contains("arm64") {
        "arm64"
    } else {
        "x64"
    };
    res = res.replace("#{arch}", simple_arch);

    // handle #{staged_path} - refers to the installation directory
    // We replace with "." so that joining to the caskroom path works correctly (avoiding absolute path override)
    res = res.replace("#{staged_path}", ".");

    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_antigravity_snippet() {
        let content = r##"
cask "antigravity-linux" do
  arch arm: "arm", intel: "x64"

  version "1.15.8,5724687216017408"
  sha256 arm64_linux:  "a39cb7...",
         x86_64_linux: "44afc..."

  url "https://edgedl.me.gvt1.com/edgedl/release2/j0qc3/antigravity/stable/#{version.csv.first}-#{version.csv.second}/linux-#{arch}/Antigravity.tar.gz"
  name "Google Antigravity"
  
  binary "#{staged_path}/Antigravity/bin/antigravity"
end
"##;
        // Test x86_64_linux
        let cask = Cask::parse(content, "x86_64_linux").unwrap();
        assert_eq!(cask.version, "1.15.8,5724687216017408");
        assert_eq!(cask.name, "Google Antigravity");
        // Verify interpolation
        assert!(cask.url.contains("1.15.8-5724687216017408"));
        assert!(cask.url.contains("linux-x64"));
    }
}
