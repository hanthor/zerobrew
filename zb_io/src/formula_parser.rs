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
            pending_url: None,
            root_url: None,
        };

        visit_node(root_node, content, &mut formula_data, PlatformContext::None);

        // Build Formula struct
        let version = formula_data.version.ok_or_else(|| Error::StoreCorruption {
            message: format!("Formula {} missing version", name),
        })?;

        if formula_data.bottles.is_empty() {
            // Check if we have a top-level URL and SHA256 to use as a fallback bottle
            // This is common for formulas that don't have explicit bottle blocks but provide pre-compiled binaries.
            if let Some(_url) = formula_data.pending_url.take() {
                // We'll need to find the sha256. In visit_node, sha256 sets the bottle.
                // If bottles is empty, it means we found a URL but no SHA256 was matched yet,
                // or visit_node logic didn't catch it.
                // Actually, my new visit_node logic inserts into bottles when it sees sha256 if pending_url is set.
                // So if it's empty, we really don't have enough info.
            }

            if formula_data.bottles.is_empty() {
                return Err(Error::StoreCorruption {
                    message: format!("Formula {} has no bottles", name),
                });
            }
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

#[derive(Debug, Default)]
struct FormulaData {
    name: String,
    version: Option<String>,
    dependencies: Vec<String>,
    bottles: BTreeMap<String, BottleFile>,
    pending_url: Option<String>,
    root_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PlatformContext {
    None,
    Linux,
    MacOS,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    // Handle if/elsif/else conditionals
    if kind == "if" || kind == "if_modifier" {
        // Check for branches and execute the first matching one
        if let Some(matching_node) = find_matching_branch(node, source) {
            process_branch_body(matching_node, source, data, context);
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
                if let Some(version) = find_string_argument(&node, source) {
                    data.version = Some(version);
                }
            }
            "depends_on" => {
                if let Some(dep) = find_string_argument(&node, source) {
                    data.dependencies.push(dep);
                }
            }
            "url" => {
                data.pending_url = find_string_argument(&node, source);
                // Also try to extract version from tag if not already set
                if data.version.is_none()
                    && let Some(tag) = find_keyword_argument(&node, source, "tag")
                {
                    data.version = Some(tag.trim_start_matches('v').to_string());
                }
            }
            "root_url" => {
                data.root_url = find_string_argument(&node, source);
            }
            "sha256" => {
                // Try to find SHA either as a direct string argument or as a value in a keyword argument
                let sha = find_string_argument(&node, source)
                    .or_else(|| find_platform_keyword_argument(&node, source).map(|(_, v)| v));

                if let Some(sha) = sha
                    && let Some(url) = &data.pending_url
                {
                    // Determine platform_tag
                    // If there's a platform keyword (like x86_64_linux: "sha"), use it
                    let platform_tag =
                        if let Some((tag, _)) = find_platform_keyword_argument(&node, source) {
                            tag
                        } else {
                            match context {
                                PlatformContext::Linux => match get_current_arch() {
                                    CpuArch::ARM64 => "arm64_linux".to_string(),
                                    _ => "x86_64_linux".to_string(),
                                },
                                PlatformContext::MacOS => match get_current_arch() {
                                    CpuArch::ARM64 => "arm64_sonoma".to_string(),
                                    _ => "sonoma".to_string(),
                                },
                                PlatformContext::None => {
                                    // Top-level url/sha256: map to current platform
                                    if cfg!(target_os = "linux") {
                                        match get_current_arch() {
                                            CpuArch::ARM64 => "arm64_linux".to_string(),
                                            _ => "x86_64_linux".to_string(),
                                        }
                                    } else {
                                        "arm64_sonoma".to_string()
                                    }
                                }
                            }
                        };
                    let url = if let Some(root) = &data.root_url {
                        // If it's a platform-specific SHA in a bottle block, use root_url + name-version.tag.bottle.tar.gz
                        format!(
                            "{}/{}-{}.{}.bottle.tar.gz",
                            root,
                            data.name,
                            data.version.as_deref().unwrap_or(""),
                            platform_tag
                        )
                    } else {
                        url.clone()
                    };

                    data.bottles
                        .insert(platform_tag, BottleFile { url, sha256: sha });
                }
            }
            "on_linux" | "on_macos" | "on_intel" | "on_arm" => {
                let new_context = match method_name {
                    "on_linux" => PlatformContext::Linux,
                    "on_macos" => PlatformContext::MacOS,
                    _ => context, // intel/arm doesn't change OS context
                };

                // Check architecture if it's an arch-specific block
                let should_visit = match method_name {
                    "on_intel" => get_current_arch() == CpuArch::X86_64,
                    "on_arm" => get_current_arch() == CpuArch::ARM64,
                    "on_linux" => cfg!(target_os = "linux"),
                    "on_macos" => cfg!(target_os = "macos"),
                    _ => true,
                };

                if should_visit {
                    // Process block body
                    if let Some(body) = node.child_by_field_name("body") {
                        visit_node(body, source, data, new_context);
                    } else {
                        // Some tree-sitter versions might not use "body" field for do blocks
                        for i in 0..node.child_count() {
                            if let Some(child) = node.child(i) {
                                let child_kind = child.kind();
                                if child_kind != "method"
                                    && child_kind != "do"
                                    && child_kind != "end"
                                {
                                    visit_node(child, source, data, new_context);
                                }
                            }
                        }
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

/// Find the matching branch node (if, elsif, or else)
fn find_matching_branch<'a>(node: Node<'a>, source: &str) -> Option<Node<'a>> {
    // Check main if branch
    if let Some(condition) = node.child_by_field_name("condition")
        && should_process_condition_node(&condition, source)
    {
        return Some(node);
    }

    // Check elsif/else branches
    find_matching_alternative(node, source)
}

fn find_matching_alternative<'a>(node: Node<'a>, source: &str) -> Option<Node<'a>> {
    for i in 0..node.child_count() {
        let child = node.child(i).unwrap();
        let kind = child.kind();

        if kind == "elsif" {
            if let Some(condition) = child.child_by_field_name("condition")
                && should_process_condition_node(&condition, source)
            {
                return Some(child);
            }
            // Recurse into potential nested branches after this elsif
            if let Some(alt) = find_matching_alternative(child, source) {
                return Some(alt);
            }
        } else if kind == "else_clause" {
            // Check if this else contains an elsif or is just the else
            for j in 0..child.child_count() {
                let inner = child.child(j).unwrap();
                let inner_kind = inner.kind();
                if inner_kind == "elsif" {
                    if let Some(condition) = inner.child_by_field_name("condition")
                        && should_process_condition_node(&condition, source)
                    {
                        return Some(inner);
                    }
                    if let Some(alt) = find_matching_alternative(inner, source) {
                        return Some(alt);
                    }
                } else if inner_kind == "else" {
                    return Some(inner);
                }
            }
        } else if kind == "else" {
            return Some(child);
        }
    }
    None
}

/// Process children of a branch node (if, elsif, or else) but STOP before next branch keyword
fn process_branch_body(node: Node, source: &str, data: &mut FormulaData, context: PlatformContext) {
    for i in 0..node.child_count() {
        let child = node.child(i).unwrap();
        let kind = child.kind();

        // Skip structural elements of the branch node itself
        if !child.is_named() {
            let text = child.utf8_text(source.as_bytes()).unwrap_or("");
            if text == "if"
                || text == "elsif"
                || text == "else"
                || text == "then"
                || text == "do"
                || text == "end"
            {
                continue;
            }
        }
        if let Some(field_name) = node.field_name_for_child(i as u32)
            && field_name == "condition"
        {
            continue;
        }

        // Also skip the else_clause or elsif named nodes themselves when processing the if/elsif body
        // as they are handled by find_matching_branch finding the correct one to call process_branch_body on.
        if kind == "else_clause" || kind == "elsif" {
            continue;
        }

        visit_node(child, source, data, context);
    }
}

/// Check if a condition node evaluates to true
fn should_process_condition_node(node: &Node, source: &str) -> bool {
    let condition_text = node.utf8_text(source.as_bytes()).unwrap_or("");
    should_process_conditional_text(condition_text)
}

/// Check if an if conditional should be processed based on Hardware::CPU and OS checks
fn should_process_conditional_text(text: &str) -> bool {
    let is_macos = cfg!(target_os = "macos");
    let is_linux = cfg!(target_os = "linux");
    let current_arch = get_current_arch();

    // Split by && and check all conditions
    for part in text.split("&&") {
        let part = part.trim();

        // OS checks
        if part.contains("OS.mac?") && !is_macos {
            return false;
        }
        if part.contains("OS.linux?") && !is_linux {
            return false;
        }

        // CPU checks
        if (part.contains("Hardware::CPU.intel?") || part.contains("Hardware::CPU.is_intel?"))
            && current_arch != CpuArch::X86_64
        {
            return false;
        }
        if (part.contains("Hardware::CPU.arm?") || part.contains("Hardware::CPU.is_arm?"))
            && current_arch != CpuArch::ARM64
        {
            return false;
        }
        if part.contains("Hardware::CPU.is_64_bit?")
            && current_arch != CpuArch::X86_64
            && current_arch != CpuArch::ARM64
        {
            // Both x86_64 and arm64 are 64-bit
            return false;
        }
    }

    // If we didn't return false, at least one part might match or it's unknown
    // Check if it's a positive match for OUR platform
    if text.contains("OS.mac?") && is_macos {
        return true;
    }
    if text.contains("OS.linux?") && is_linux {
        return true;
    }
    if text.contains("Hardware::CPU.intel?") && current_arch == CpuArch::X86_64 {
        return true;
    }
    if text.contains("Hardware::CPU.arm?") && current_arch == CpuArch::ARM64 {
        return true;
    }
    if text.contains("Hardware::CPU.is_64_bit?") {
        return true; // Both supported arches are 64-bit
    }

    // If no known checks found, default to true
    true
}

fn find_keyword_argument(node: &Node, source: &str, name: &str) -> Option<String> {
    if let Some(args) = node.child_by_field_name("arguments") {
        for i in 0..args.child_count() {
            let child = args.child(i).unwrap();
            let kind = child.kind();
            if (kind == "keyword_argument" || kind == "pair")
                && let Some(name_node) = child
                    .child_by_field_name("name")
                    .or_else(|| child.child_by_field_name("key"))
            {
                let key = name_node.utf8_text(source.as_bytes()).unwrap_or("");
                let key = key.trim_end_matches(':');
                if key == name
                    && let Some(value_node) = child.child_by_field_name("value")
                {
                    let text = value_node.utf8_text(source.as_bytes()).unwrap_or("");
                    return Some(text.trim_matches('"').trim_matches('\'').to_string());
                }
            }
        }
    }
    None
}

fn find_platform_keyword_argument(node: &Node, source: &str) -> Option<(String, String)> {
    if let Some(args) = node.child_by_field_name("arguments") {
        for i in 0..args.child_count() {
            let child = args.child(i).unwrap();
            let kind = child.kind();
            if (kind == "keyword_argument" || kind == "pair")
                && let Some(name_node) = child
                    .child_by_field_name("name")
                    .or_else(|| child.child_by_field_name("key"))
            {
                let name = name_node
                    .utf8_text(source.as_bytes())
                    .unwrap_or("")
                    .to_string();
                let name = name.trim_end_matches(':').to_string();
                // Skip known non-platform keywords
                if name == "cellar"
                    || name == "tag"
                    || name == "revision"
                    || name == "branch"
                    || name == "using"
                {
                    continue;
                }
                if let Some(value_node) = child.child_by_field_name("value") {
                    let value = value_node.utf8_text(source.as_bytes()).unwrap_or("");
                    return Some((name, value.trim_matches('"').trim_matches('\'').to_string()));
                }
            }
        }
    }
    None
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

    #[test]
    fn test_parse_complex_conditionals() {
        let content = r#"
class ComplexFormula < Formula
  version "2.0.0"
  
  if OS.mac? && Hardware::CPU.arm?
    url "https://example.com/mac-arm.tgz"
    sha256 "mac-arm-sha"
  elsif OS.mac?
    url "https://example.com/mac-intel.tgz"
    sha256 "mac-intel-sha"
  elsif OS.linux? && Hardware::CPU.arm?
    url "https://example.com/linux-arm.tgz"
    sha256 "linux-arm-sha"
  else
    url "https://example.com/linux-intel.tgz"
    sha256 "linux-intel-sha"
  end
end
"#;

        let formula = FormulaParser::parse(content, "complex").unwrap();

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let bottle = formula.bottle.stable.files.get("x86_64_linux").unwrap();
            assert_eq!(bottle.url, "https://example.com/linux-intel.tgz");
            assert_eq!(bottle.sha256, "linux-intel-sha");
        }

        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            let bottle = formula.bottle.stable.files.get("arm64_sonoma").unwrap();
            assert_eq!(bottle.url, "https://example.com/mac-arm.tgz");
            assert_eq!(bottle.sha256, "mac-arm-sha");
        }
    }

    #[test]
    fn test_parse_on_arch_blocks() {
        let content = r#"
class ArchFormula < Formula
  version "1.1.0"
  
  on_linux do
    on_intel do
      url "https://linux-intel.tar.gz"
      sha256 "lintel"
    end
    on_arm do
      url "https://linux-arm.tar.gz"
      sha256 "larm"
    end
  end
end
"#;

        let formula = FormulaParser::parse(content, "arch").unwrap();

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let bottle = formula.bottle.stable.files.get("x86_64_linux").unwrap();
            assert_eq!(bottle.url, "https://linux-intel.tar.gz");
        }
    }
}
