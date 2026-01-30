use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::Node;
use zb_core::{Error, Formula, formula::*};

pub struct FormulaParser;

impl FormulaParser {
    pub fn parse(content: &str, name: &str) -> Result<Formula, Error> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(tree_sitter_ruby::language())
            .map_err(|e| Error::StoreCorruption {
                message: format!("Failed to load Ruby language for formula parsing: {}", e),
            })?;

        let tree = parser.parse(content, None).ok_or(Error::StoreCorruption {
            message: "Failed to parse formula".to_string(),
        })?;

        let root_node = tree.root_node();
        let mut formula_data = FormulaData {
            name: name.to_string(),
            version: None,
            dependencies: Vec::new(),
            bottles: BTreeMap::new(),
        };

        visit_node(root_node, content, &mut formula_data, PlatformContext::None);

        // Build Formula struct
        let version = formula_data.version.ok_or_else(|| Error::StoreCorruption {
            message: format!("Formula {} missing version", name),
        })?;

        if formula_data.bottles.is_empty() {
            return Err(Error::StoreCorruption {
                message: format!("Formula {} has no bottles", name),
            });
        }

        Ok(Formula {
            name: formula_data.name,
            versions: Versions { stable: version },
            dependencies: formula_data.dependencies,
            bottle: Bottle {
                stable: BottleStable {
                    files: formula_data.bottles,
                    rebuild: 0,
                },
            },
            revision: 0,
        })
    }

    pub fn parse_file<P: AsRef<Path>>(path: P, name: &str) -> Result<Formula, Error> {
        let content = std::fs::read_to_string(path).map_err(|e| Error::StoreCorruption {
            message: format!("Failed to read formula file: {}", e),
        })?;
        Self::parse(&content, name)
    }
}

#[derive(Debug)]
struct FormulaData {
    name: String,
    version: Option<String>,
    dependencies: Vec<String>,
    bottles: BTreeMap<String, BottleFile>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PlatformContext {
    None,
    Linux,
    MacOS,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CpuArch {
    X86_64,
    ARM64,
}

fn get_current_arch() -> CpuArch {
    #[cfg(target_arch = "x86_64")]
    return CpuArch::X86_64;

    #[cfg(target_arch = "aarch64")]
    return CpuArch::ARM64;

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return CpuArch::X86_64; // Default fallback
}

fn visit_node(node: Node, source: &str, data: &mut FormulaData, context: PlatformContext) {
    let kind = node.kind();

    // Handle if conditionals with Hardware::CPU checks
    if kind == "if" || kind == "if_modifier" {
        if should_process_conditional(&node, source) {
            // Process the body of the if statement
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    visit_node(child, source, data, context);
                }
            }
        }
        return; // Don't recursively visit children again
    }

    // Handle method calls
    if (kind == "method_call" || kind == "call")
        && let Some(method_node) = node.child_by_field_name("method")
    {
        let method_name = method_node.utf8_text(source.as_bytes()).unwrap_or("");

        match method_name {
            "version" => {
                if let Some(arg) = find_string_argument(&node, source) {
                    data.version = Some(arg);
                }
            }
            "depends_on" => {
                if let Some(dep) = find_string_argument(&node, source) {
                    data.dependencies.push(dep);
                }
            }
            "url" => {
                if let Some(url) = find_string_argument(&node, source) {
                    // Store URL temporarily, will be matched with sha256
                    if let Some(parent) = node.parent()
                        && let Some(sha_node) = find_sibling_sha256(&parent, source)
                    {
                        let platform_tag = match context {
                            PlatformContext::Linux => "x86_64_linux",
                            PlatformContext::MacOS => "arm64_sonoma", // Default to ARM Mac
                            PlatformContext::None => "x86_64_linux",  // Default
                        };
                        data.bottles.insert(
                            platform_tag.to_string(),
                            BottleFile {
                                url,
                                sha256: sha_node,
                            },
                        );
                    }
                }
            }
            "on_linux" => {
                // Visit children with Linux context
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        visit_node(child, source, data, PlatformContext::Linux);
                    }
                }
                return;
            }
            "on_macos" => {
                // Visit children with MacOS context
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        visit_node(child, source, data, PlatformContext::MacOS);
                    }
                }
                return;
            }
            _ => {}
        }
    }

    // Recursively visit children
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            visit_node(child, source, data, context);
        }
    }
}

/// Check if an if conditional should be processed based on Hardware::CPU checks
fn should_process_conditional(node: &Node, source: &str) -> bool {
    let current_arch = get_current_arch();

    // Get the condition node
    let condition = if let Some(cond) = node.child_by_field_name("condition") {
        cond
    } else {
        // No condition found, process by default
        return true;
    };

    // Check if condition contains Hardware::CPU checks
    let condition_text = condition.utf8_text(source.as_bytes()).unwrap_or("");

    // Check for intel vs arm
    if condition_text.contains("Hardware::CPU.intel?") {
        return current_arch == CpuArch::X86_64;
    }

    if condition_text.contains("Hardware::CPU.arm?") {
        return current_arch == CpuArch::ARM64;
    }

    // If no Hardware::CPU check found, process by default
    true
}

fn find_string_argument(node: &Node, source: &str) -> Option<String> {
    // Look for argument_list -> string
    if let Some(args) = node.child_by_field_name("arguments") {
        for i in 0..args.child_count() {
            if let Some(child) = args.child(i)
                && (child.kind() == "string" || child.kind() == "simple_string")
            {
                let text = child.utf8_text(source.as_bytes()).ok()?;
                // Remove quotes
                return Some(text.trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    None
}

fn find_sibling_sha256(parent: &Node, source: &str) -> Option<String> {
    // Look for a sibling method call with name "sha256"
    for i in 0..parent.child_count() {
        if let Some(child) = parent.child(i)
            && (child.kind() == "method_call" || child.kind() == "call")
            && let Some(method) = child.child_by_field_name("method")
        {
            let method_name = method.utf8_text(source.as_bytes()).unwrap_or("");
            if method_name == "sha256" {
                return find_string_argument(&child, source);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_formula() {
        let content = r#"
class TestFormula < Formula
  version "1.0.0"
  depends_on "foo"
  
  on_linux do
    url "https://example.com/test-linux.tar.gz"
    sha256 "abc123"
  end
end
"#;

        let formula = FormulaParser::parse(content, "test").unwrap();
        assert_eq!(formula.name, "test");
        assert_eq!(formula.versions.stable, "1.0.0");
        assert_eq!(formula.dependencies, vec!["foo"]);
        assert!(formula.bottle.stable.files.contains_key("x86_64_linux"));
    }
}
