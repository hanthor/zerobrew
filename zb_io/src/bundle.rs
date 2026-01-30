use std::path::Path;
use zb_core::Error;

#[derive(Debug, PartialEq, Eq)]
pub struct Bundle {
    pub taps: Vec<String>,
    pub brews: Vec<String>,
    pub casks: Vec<String>,
    pub flatpaks: Vec<String>,
}

pub struct BundleParser;

impl BundleParser {
    pub fn parse(content: &str) -> Result<Bundle, Error> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(tree_sitter_ruby::language())
            .map_err(|e| Error::StoreCorruption {
                message: format!("Failed to load Ruby language for Brewfile parsing: {}", e),
            })?;

        let tree = parser.parse(content, None).ok_or(Error::StoreCorruption {
            message: "Failed to parse Brewfile".to_string(),
        })?;

        let root_node = tree.root_node();
        let mut bundle = Bundle {
            taps: Vec::new(),
            brews: Vec::new(),
            casks: Vec::new(),
            flatpaks: Vec::new(),
        };

        visit_node(root_node, content, &mut bundle);

        Ok(bundle)
    }

    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Bundle, Error> {
        let content = std::fs::read_to_string(path).map_err(|e| Error::StoreCorruption {
            message: format!("Failed to read Brewfile: {}", e),
        })?;
        Self::parse(&content)
    }
}

fn visit_node(node: tree_sitter::Node, source: &str, bundle: &mut Bundle) {
    let kind = node.kind();

    if (kind == "method_call" || kind == "command_call" || kind == "command" || kind == "call")
        && let Some(method_node) = node.child_by_field_name("method")
    {
        let method_name = &source[method_node.byte_range()];

        if let Some(args_node) = node.child_by_field_name("arguments")
            && let Some(arg) = extract_first_string_arg(args_node, source)
        {
            match method_name {
                "tap" => bundle.taps.push(arg),
                "brew" => bundle.brews.push(arg),
                "cask" => bundle.casks.push(arg),
                "flatpak" => bundle.flatpaks.push(arg),
                _ => {}
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_node(child, source, bundle);
    }
}

fn extract_first_string_arg(args_node: tree_sitter::Node, source: &str) -> Option<String> {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "string" {
            // Ruby strings can have multiple parts (interpolation, etc.)
            // For now, let's just grab the content if it's a simple string
            let mut inner_cursor = child.walk();
            for inner in child.children(&mut inner_cursor) {
                if inner.kind() == "string_content" {
                    return Some(source[inner.byte_range()].to_string());
                }
            }
            // Fallback for empty strings or some other edge cases
            let text = &source[child.byte_range()];
            return Some(text.trim_matches(|c| c == '\'' || c == '"').to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_brewfile() {
        let content = r#"
            tap "homebrew/core"
            brew "bat"
            cask "firefox"
            flatpak "org.mozilla.firefox"
        "#;
        let bundle = BundleParser::parse(content).unwrap();

        assert_eq!(bundle.taps, vec!["homebrew/core"]);
        assert_eq!(bundle.brews, vec!["bat"]);
        assert_eq!(bundle.casks, vec!["firefox"]);
        assert_eq!(bundle.flatpaks, vec!["org.mozilla.firefox"]);
    }

    #[test]
    fn test_parse_mixed_quotes_and_comments() {
        let content = r#"
            # This is a comment
            tap 'homebrew/services'
            brew "ripgrep" # Inline comment
            
            # Another comment
            cask "iterm2"
        "#;

        let bundle = BundleParser::parse(content).unwrap();

        assert!(bundle.taps.contains(&"homebrew/services".to_string()));
        assert_eq!(bundle.brews, vec!["ripgrep"]);
        assert_eq!(bundle.casks, vec!["iterm2"]);
    }

    #[test]
    fn test_ignore_unknown_commands() {
        let content = r#"
            gem "rails"
            brew "git"
            mas "Xcode", id: 497799835
        "#;
        let bundle = BundleParser::parse(content).unwrap();

        assert_eq!(bundle.brews, vec!["git"]);
        assert!(bundle.taps.is_empty());
        assert!(bundle.flatpaks.is_empty());
    }
}
