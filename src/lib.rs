//! Kotlin parser plugin, full-parse mode.
//!
//! This parser intentionally avoids a host tree-sitter dependency.  It emits a
//! compact semantic tree for common Kotlin declarations so the advertised
//! playground example can be parsed in a default install.

use intentdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

struct KotlinParser;

#[derive(Debug, Clone)]
struct SourceLine {
    number: u32,
    text: String,
    trimmed: String,
}

fn lines(source: &str) -> Vec<SourceLine> {
    source
        .lines()
        .enumerate()
        .map(|(i, text)| SourceLine {
            number: i as u32,
            text: text.to_string(),
            trimmed: text.trim().to_string(),
        })
        .collect()
}

fn clean_name(raw: &str) -> String {
    raw.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.'))
        .to_string()
}

fn word_after_keyword(line: &str, keyword: &str) -> String {
    let rest = line.trim_start_matches(keyword).trim();
    clean_name(
        rest.split(|c: char| c == '(' || c == ':' || c == '<' || c.is_whitespace())
            .next()
            .unwrap_or(""),
    )
}

fn function_name(line: &str) -> String {
    let rest = line.trim_start_matches("fun").trim();
    let before_paren = rest.split('(').next().unwrap_or("").trim();
    let name = before_paren
        .split_whitespace()
        .last()
        .unwrap_or(before_paren)
        .split('.')
        .last()
        .unwrap_or(before_paren);
    clean_name(name)
}

fn property_name(line: &str) -> String {
    let rest = line
        .trim_start_matches("val ")
        .trim_start_matches("var ")
        .trim();
    clean_name(
        rest.split(|c: char| c == ':' || c == '=' || c.is_whitespace())
            .next()
            .unwrap_or(""),
    )
}

fn brace_delta(text: &str) -> i32 {
    let mut delta = 0;
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

fn block_end(lines: &[SourceLine], start: usize) -> usize {
    let mut depth = brace_delta(&lines[start].trimmed);
    if depth <= 0 {
        return start;
    }
    let mut end = start;
    while end + 1 < lines.len() {
        end += 1;
        depth += brace_delta(&lines[end].trimmed);
        if depth <= 0 {
            break;
        }
    }
    end
}

fn leaf(id: &str, node_type: &str, label: &str, line: &SourceLine) -> SemanticNode {
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        line.number,
        0,
        line.number,
        line.text.len() as u32,
        "",
    )
    .build()
}

fn statement_type(trimmed: &str) -> &'static str {
    if trimmed.starts_with("return ") {
        "return_statement"
    } else if trimmed.contains('=') && !trimmed.starts_with("println") {
        "assignment"
    } else if trimmed.contains('(') && trimmed.contains(')') {
        "call_expression"
    } else {
        "expression_statement"
    }
}

fn body_children(
    id: &str,
    block_lines: &[SourceLine],
    start: usize,
    end: usize,
) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    let header = &block_lines[start];
    children.push(leaf(
        &format!("{}.0", id),
        "signature",
        &header.trimmed,
        header,
    ));

    if start == end {
        if let Some((_, rhs)) = header.trimmed.split_once('=') {
            let label = rhs.trim();
            children.push(leaf(
                &format!("{}.1", id),
                "expression_statement",
                label,
                header,
            ));
        }
        return children;
    }

    let mut child_index = 1;
    for line in block_lines.iter().take(end).skip(start + 1) {
        if line.trimmed.is_empty() || line.trimmed == "}" {
            continue;
        }
        children.push(leaf(
            &format!("{}.{}", id, child_index),
            statement_type(&line.trimmed),
            &line.trimmed,
            line,
        ));
        child_index += 1;
    }
    children
}

fn declaration_node(
    id: &str,
    node_type: &str,
    label: &str,
    source_lines: &[SourceLine],
    start: usize,
    end: usize,
) -> SemanticNode {
    let children = body_children(id, source_lines, start, end);
    let first = &source_lines[start];
    let last = &source_lines[end];
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        first.number,
        0,
        last.number,
        last.text.len() as u32,
        "",
    )
    .children(children)
    .build()
}

fn parse_source(source: &str) -> SemanticNode {
    let source_lines = lines(source);
    let mut children = Vec::new();
    let mut i = 0usize;
    let mut child_index = 0usize;

    while i < source_lines.len() {
        let line = &source_lines[i];
        let trimmed = line.trimmed.as_str();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        let id = format!("0.{}", child_index);
        if trimmed.starts_with("package ") {
            children.push(leaf(
                &id,
                "package_header",
                trimmed.trim_start_matches("package ").trim(),
                line,
            ));
            i += 1;
        } else if trimmed.starts_with("import ") {
            children.push(leaf(
                &id,
                "import_header",
                trimmed.trim_start_matches("import ").trim(),
                line,
            ));
            i += 1;
        } else if trimmed.starts_with("class ") {
            let end = block_end(&source_lines, i);
            children.push(declaration_node(
                &id,
                "class_declaration",
                &word_after_keyword(trimmed, "class"),
                &source_lines,
                i,
                end,
            ));
            i = end + 1;
        } else if trimmed.starts_with("object ") {
            let end = block_end(&source_lines, i);
            children.push(declaration_node(
                &id,
                "object_declaration",
                &word_after_keyword(trimmed, "object"),
                &source_lines,
                i,
                end,
            ));
            i = end + 1;
        } else if trimmed.starts_with("interface ") {
            let end = block_end(&source_lines, i);
            children.push(declaration_node(
                &id,
                "interface_declaration",
                &word_after_keyword(trimmed, "interface"),
                &source_lines,
                i,
                end,
            ));
            i = end + 1;
        } else if trimmed.starts_with("fun ") {
            let end = block_end(&source_lines, i);
            children.push(declaration_node(
                &id,
                "function_declaration",
                &function_name(trimmed),
                &source_lines,
                i,
                end,
            ));
            i = end + 1;
        } else if trimmed.starts_with("val ") || trimmed.starts_with("var ") {
            children.push(leaf(
                &id,
                "property_declaration",
                &property_name(trimmed),
                line,
            ));
            i += 1;
        } else {
            children.push(leaf(&id, statement_type(trimmed), trimmed, line));
            i += 1;
        }
        child_index += 1;
    }

    let end_line = source.lines().count().max(1) as u32;
    SemanticNodeBuilder::new("0", "source_file", "source_file", 1, 0, end_line, 0, "")
        .children(children)
        .build()
}

fn process_impl(source: &str) -> String {
    match serde_json::to_string(&parse_source(source)) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for KotlinParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "kotlin".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".kt") || lower.ends_with(".kts") {
            "kotlin".to_string()
        } else {
            String::new()
        }
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "fun greet(name: String) {\n    println(\"Hello, \" + name)\n}\n\nfun add(a: Int, b: Int): Int {\n    return a + b\n}\n".to_string(),
            new: "fun greet(name: String) {\n    println(\"Hello, $name!\")\n}\n\nfun add(x: Int, y: Int): Int = x + y\n\nfun multiply(x: Int, y: Int): Int = x * y\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        Vec::new()
    }
    fn language_ids() -> Vec<String> {
        vec!["kotlin".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(KotlinParser);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!KotlinParser::grammar_id().is_empty());
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            KotlinParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        assert!(KotlinParser::language_ids().contains(&KotlinParser::grammar_id()));
    }

    #[test]
    fn detect_language_known_ext() {
        assert_eq!(
            KotlinParser::detect_language("test.kt".to_string(), "".to_string()),
            "kotlin"
        );
    }

    #[test]
    fn detect_language_unknown_ext() {
        assert_eq!(
            KotlinParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string()),
            ""
        );
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        t::assert_valid_json(&process_impl(""), "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        t::assert_valid_json(&process_impl("   \n  "), "process(whitespace)");
    }

    #[test]
    fn playground_example_produces_functions() {
        let example = <KotlinParser as Guest>::example("kotlin".to_string());
        let out = process_impl(&example.new);
        t::assert_valid_json(&out, "kotlin example");
        t::assert_no_error(&out, "kotlin example");
        assert!(out.contains("function_declaration"));
        assert!(out.contains("multiply"));
    }
}
