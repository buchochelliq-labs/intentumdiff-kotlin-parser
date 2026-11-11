//! Kotlin parser plugin, full-parse mode.
//!
//! This parser intentionally avoids a host tree-sitter dependency.  It emits a
//! compact semantic tree for common Kotlin declarations so the advertised
//! playground example can be parsed in a default install.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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
    /// `text` with comments and literals blanked: only structural code, at
    /// the same byte offsets.  Used for brace matching.
    code: String,
}

/// Lexical class of a source byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteClass {
    /// Executable code outside any literal.
    Code,
    /// Part of a comment (`// ...`, nested `/* ... */`, KDoc).
    Comment,
    /// Part of a string or character literal, including its delimiters and
    /// any `${...}` template expression nested in it.
    Literal,
}

/// One open lexical context on the scanner stack.
#[derive(Debug, Clone, Copy)]
enum Frame {
    /// `"..."`: escapes and templates.
    Str,
    /// `"""..."""`: templates, no escapes, may span lines.
    RawStr,
    /// `${...}` inside a string; `depth` counts unmatched `{` in the expression.
    Template { depth: usize },
}

/// Classify every byte of `source`.  The scanner keeps a stack of
/// string/template contexts so a string literal nested in a template
/// expression (`"a ${m["k"]} b"`), braces inside the expression
/// (`"${xs.map { "}" }}"`), raw strings with templates and escapes (`\"`,
/// `\$`) are all tracked; comment delimiters are recognised only in code
/// (top level or inside a template expression).
fn classify(source: &str) -> Vec<ByteClass> {
    let bytes = source.as_bytes();
    let mut class = vec![ByteClass::Code; bytes.len()];
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0usize;
    let mark = |class: &mut [ByteClass], from: usize, to: usize, c: ByteClass| {
        for slot in &mut class[from..to.min(bytes.len())] {
            *slot = c;
        }
    };
    while i < bytes.len() {
        let b = bytes[i];
        match stack.last().copied() {
            None | Some(Frame::Template { .. }) => {
                let here = if stack.is_empty() {
                    ByteClass::Code
                } else {
                    ByteClass::Literal
                };
                if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    let from = i;
                    while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                        i += 1;
                    }
                    mark(&mut class, from, i, ByteClass::Comment);
                } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    let from = i;
                    let mut depth = 0usize;
                    while i < bytes.len() {
                        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                            depth += 1;
                            i += 2;
                        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                            depth -= 1;
                            i += 2;
                            if depth == 0 {
                                break;
                            }
                        } else {
                            i += 1;
                        }
                    }
                    mark(&mut class, from, i, ByteClass::Comment);
                } else if bytes[i..].starts_with(b"\"\"\"") {
                    stack.push(Frame::RawStr);
                    mark(&mut class, i, i + 3, ByteClass::Literal);
                    i += 3;
                } else if b == b'"' {
                    stack.push(Frame::Str);
                    class[i] = ByteClass::Literal;
                    i += 1;
                } else if b == b'\'' {
                    // Character literal: `'x'`, `'\''`, `'A'`, `'"'`.
                    let from = i;
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        let c = bytes[i];
                        i += 1;
                        if c == b'\\' && i < bytes.len() && bytes[i] != b'\n' {
                            i += 1;
                        } else if c == b'\'' {
                            break;
                        }
                    }
                    mark(&mut class, from, i, ByteClass::Literal);
                } else {
                    class[i] = here;
                    if let Some(Frame::Template { depth }) = stack.last_mut() {
                        if b == b'{' {
                            *depth += 1;
                        } else if b == b'}' {
                            if *depth == 0 {
                                stack.pop();
                            } else {
                                *depth -= 1;
                            }
                        }
                    }
                    i += 1;
                }
            }
            Some(Frame::Str) => {
                if b == b'\n' || b == b'\r' {
                    // Unterminated single-line string: recover at the line end.
                    stack.pop();
                    continue;
                }
                class[i] = ByteClass::Literal;
                if b == b'\\' && i + 1 < bytes.len() && bytes[i + 1] != b'\n' {
                    let width = utf8_width(bytes[i + 1]);
                    mark(&mut class, i, i + 1 + width, ByteClass::Literal);
                    i += 1 + width;
                } else if b == b'"' {
                    stack.pop();
                    i += 1;
                } else if b == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    class[i + 1] = ByteClass::Literal;
                    stack.push(Frame::Template { depth: 0 });
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Some(Frame::RawStr) => {
                class[i] = ByteClass::Literal;
                if bytes[i..].starts_with(b"\"\"\"") {
                    // Closing delimiter plus any extra trailing quotes belong to the literal.
                    while i < bytes.len() && bytes[i] == b'"' {
                        class[i] = ByteClass::Literal;
                        i += 1;
                    }
                    stack.pop();
                } else if b == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    class[i + 1] = ByteClass::Literal;
                    stack.push(Frame::Template { depth: 0 });
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
    }
    class
}

fn utf8_width(lead: u8) -> usize {
    match lead {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// Replace every byte whose class is in `blank` with a space, keeping line
/// breaks.  Each blanked character becomes as many spaces as its UTF-8 length,
/// so rows and byte columns are unchanged.
fn blank_classes(source: &str, class: &[ByteClass], blank: &[ByteClass]) -> String {
    let out: Vec<u8> = source
        .bytes()
        .zip(class)
        .map(|(b, c)| {
            if b != b'\n' && b != b'\r' && blank.contains(c) {
                b' '
            } else {
                b
            }
        })
        .collect();
    // Literals and comments always cover whole UTF-8 sequences, so the result
    // is valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| source.to_string())
}

/// Replace every Kotlin comment with spaces so comments never reach semantic
/// labels.  String and character literals (including nested template strings)
/// are retained, so `//`, `/*` inside them are kept.
#[cfg(test)]
fn mask_comments(source: &str) -> String {
    blank_classes(source, &classify(source), &[ByteClass::Comment])
}

fn lines(source: &str) -> Vec<SourceLine> {
    let class = classify(source);
    let masked = blank_classes(source, &class, &[ByteClass::Comment]);
    let code = blank_classes(source, &class, &[ByteClass::Comment, ByteClass::Literal]);
    masked
        .lines()
        .zip(code.lines())
        .enumerate()
        .map(|(i, (text, code))| SourceLine {
            number: i as u32,
            text: text.to_string(),
            trimmed: text.trim().to_string(),
            code: code.to_string(),
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
        .next_back()
        .unwrap_or(before_paren)
        .split('.')
        .next_back()
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

fn brace_delta(code: &str) -> i32 {
    brace_scan(code, 0).0
}

/// Scan the structural `code` view of a line (literals and comments already
/// blanked) starting at brace depth `depth`.  Returns the net brace delta and
/// the byte offset of the first `}` that brings the running depth to zero or
/// below, if any.
fn brace_scan(code: &str, depth: i32) -> (i32, Option<usize>) {
    let mut delta = 0;
    let mut close = None;
    for (idx, b) in code.bytes().enumerate() {
        match b {
            b'{' => delta += 1,
            b'}' => {
                delta -= 1;
                if close.is_none() && depth + delta <= 0 {
                    close = Some(idx);
                }
            }
            _ => {}
        }
    }
    (delta, close)
}

/// True when the structural content of `code` is only closing braces, e.g.
/// `}` or `} }`: block structure, not a statement.
fn only_closing_braces(code: &str) -> bool {
    let t = code.trim();
    !t.is_empty() && t.bytes().all(|b| b == b'}' || b.is_ascii_whitespace())
}

fn block_end(lines: &[SourceLine], start: usize) -> usize {
    let mut depth = brace_delta(&lines[start].code);
    if depth <= 0 {
        return start;
    }
    let mut end = start;
    while end + 1 < lines.len() {
        end += 1;
        depth += brace_delta(&lines[end].code);
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
    let mut depth = brace_delta(&header.code);
    for line in block_lines.iter().take(end).skip(start + 1) {
        depth += brace_delta(&line.code);
        if line.trimmed.is_empty() || only_closing_braces(&line.code) {
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

    // The closing row can carry body tokens before the closing brace
    // (`  return 1 }`) and declaration-level tokens after it (`} ; foo()`).
    // Slice it at the closing brace instead of dropping the whole row.
    let last = &block_lines[end];
    // `close` indexes a structural `}` (ASCII, in code), so it is a char
    // boundary in both views.
    let (before, after) = match brace_scan(&last.code, depth).1 {
        Some(close) => ((0, close), (close + 1, last.text.len())),
        None => ((0, last.text.len()), (last.text.len(), last.text.len())),
    };
    for (offset, to) in [before, after] {
        let segment = &last.text[offset..to];
        let label = segment.trim();
        // Skip blank segments and purely structural closing braces
        // (`  } }`), matching closing braces on their own rows.
        if label.is_empty() || only_closing_braces(&last.code[offset..to]) {
            continue;
        }
        let lead = segment.len() - segment.trim_start().len();
        let start_col = (offset + lead) as u32;
        children.push(
            SemanticNodeBuilder::new(
                format!("{}.{}", id, child_index),
                statement_type(label),
                label,
                last.number,
                start_col,
                last.number,
                start_col + label.len() as u32,
                "",
            )
            .build(),
        );
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
    use intentumdiff_plugin_sdk::testing as t;

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

    /// (node_type, label, start_line, start_col, end_line, end_col) of every node, pre-order.
    fn facts(source: &str) -> Vec<(String, String, u64, u64, u64, u64)> {
        fn walk(v: &serde_json::Value, out: &mut Vec<(String, String, u64, u64, u64, u64)>) {
            let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
            let n = |k: &str| v["position"][k].as_u64().unwrap_or(u64::MAX);
            out.push((
                s("node_type"),
                s("label"),
                n("start_line"),
                n("start_col"),
                n("end_line"),
                n("end_col"),
            ));
            if let Some(children) = v["children"].as_array() {
                for c in children {
                    walk(c, out);
                }
            }
        }
        let out = process_impl(source);
        t::assert_valid_json(&out, "kotlin facts");
        t::assert_no_error(&out, "kotlin facts");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let mut acc = Vec::new();
        walk(&v, &mut acc);
        acc
    }

    /// Position-free payload: (node_type, label) pairs.
    fn payload(source: &str) -> Vec<(String, String)> {
        facts(source)
            .into_iter()
            .map(|(ty, label, ..)| (ty, label))
            .collect()
    }

    fn find<'a>(
        facts: &'a [(String, String, u64, u64, u64, u64)],
        label: &str,
    ) -> &'a (String, String, u64, u64, u64, u64) {
        facts
            .iter()
            .find(|f| f.1 == label)
            .unwrap_or_else(|| panic!("no node labelled {label:?} in {facts:?}"))
    }

    // ---- kotlin-parser#4: body statements on the closing-brace row ----

    #[test]
    fn closing_row_body_statement_is_retained_issue_4() {
        let old = "fun f(): Int {\n  return 1 }\n";
        let new = "fun f(): Int {\n  return 2 }\n";
        let o = facts(old);
        let n = facts(new);
        assert_eq!(
            find(&o, "return 1"),
            &("return_statement".into(), "return 1".into(), 1, 2, 1, 10)
        );
        assert_eq!(
            find(&n, "return 2"),
            &("return_statement".into(), "return 2".into(), 1, 2, 1, 10)
        );
        assert_ne!(payload(old), payload(new));
    }

    #[test]
    fn closing_brace_on_own_line_adds_no_extra_node() {
        let p = payload("fun f(): Int {\n  return 1\n}\n");
        assert_eq!(
            p,
            vec![
                ("source_file".into(), "source_file".into()),
                ("function_declaration".into(), "f".into()),
                ("signature".into(), "fun f(): Int {".into()),
                ("return_statement".into(), "return 1".into()),
            ]
        );
    }

    #[test]
    fn closing_row_quoted_delimiters_are_retained() {
        let o = facts("fun f(): String {\n  return \"}\" }\n");
        assert_eq!(find(&o, "return \"}\"").0, "return_statement");
        let o = facts("fun f(): String {\n  return \"// x /* y\" }\n");
        assert_eq!(find(&o, "return \"// x /* y\"").0, "return_statement");
        assert_ne!(
            payload("fun f(): String {\n  return \"}\" }\n"),
            payload("fun f(): String {\n  return \"{\" }\n")
        );
    }

    #[test]
    fn closing_row_nested_member_and_trailing_tokens_are_retained() {
        let o = facts("class A {\n  fun g() = 1 }\n");
        assert_eq!(find(&o, "fun g() = 1").0, "assignment");
        let o = facts("fun f() {\n  g() }; h()\n");
        assert_eq!(find(&o, "g()").0, "call_expression");
        assert!(o.iter().any(|f| f.1 == "; h()"), "{o:?}");
    }

    #[test]
    fn closing_row_whitespace_only_change_is_layout() {
        assert_eq!(
            payload("fun f(): Int {\n  return 1 }\n"),
            payload("fun f(): Int {\n      return 1    }\n")
        );
    }

    // ---- kotlin-parser#5: ordinary comments are trivia ----

    #[test]
    fn ordinary_line_comment_is_not_an_expression_issue_5() {
        let old = "fun answer(): Int = 1\n";
        let new = "// Ordinary comment\nfun answer(): Int = 1\n";
        assert_eq!(payload(old), payload(new));
        let n = facts(new);
        assert!(!n.iter().any(|f| f.1.contains("Ordinary")), "{n:?}");
        // The declaration keeps its real source row (row 1, 0-based).
        assert_eq!(find(&n, "answer").2, 1);
    }

    #[test]
    fn trailing_block_and_kdoc_comments_are_trivia() {
        let base = payload("fun f(): Int {\n  return 1\n}\n");
        for src in [
            "fun f(): Int { // c\n  return 1 // trailing\n}\n",
            "/** KDoc */\nfun f(): Int {\n  return 1\n}\n",
            "/* outer /* nested } */ still comment { */\nfun f(): Int {\n  return 1\n}\n",
            "fun f(): Int {\n  // }\n  return 1\n}\n",
            "fun f(): Int {\n  return /* é */ 1\n}\n",
        ] {
            let got = payload(src);
            let got: Vec<_> = got
                .into_iter()
                .map(|(t, l)| (t, l.split_whitespace().collect::<Vec<_>>().join(" ")))
                .collect();
            assert_eq!(got, base, "source: {src:?}");
        }
    }

    #[test]
    fn comment_masking_preserves_rows_and_byte_columns() {
        let src = "/* é */ fun a() = 1\n";
        assert_eq!(mask_comments(src).len(), src.len());
        let f = facts(src);
        let sig = find(&f, "fun a() = 1");
        assert_eq!((sig.2, sig.4, sig.5), (0, 0, src.trim_end().len() as u64));
    }

    #[test]
    fn meaningful_edit_next_to_comment_still_changes() {
        assert_ne!(
            payload("// c\nfun answer(): Int = 1\n"),
            payload("// c\nfun answer(): Int = 2\n")
        );
    }

    #[test]
    fn comment_markers_inside_strings_are_kept() {
        let f = facts("val url = \"http://x\"\nfun s(): String = \"/* a */\"\n");
        assert!(f.iter().any(|x| x.1 == "\"/* a */\""), "{f:?}");
        assert_ne!(
            payload("fun s(): String = \"a // b\"\n"),
            payload("fun s(): String = \"a // c\"\n")
        );
    }

    // ---- PR #6 review: nested string literals inside templates ----

    #[test]
    fn nested_string_in_template_keeps_outer_string_open() {
        // `"${"http://a"}"`: the inner quote must not close the outer string,
        // so `//a` is string content, not a line comment.
        let a = "fun f() = \"${\"http://a\"}\"\n";
        let b = "fun f() = \"${\"http://b\"}\"\n";
        assert_eq!(mask_comments(a), a);
        assert_ne!(payload(a), payload(b));
        let a = "fun f(m: Map<String, String>) = \"a ${m[\"k\"]} // b\"\n";
        assert_eq!(mask_comments(a), a);
        // A real comment after the template string is still trivia.
        assert_eq!(
            mask_comments("val s = \"${\"x\"}\" // c\n"),
            "val s = \"${\"x\"}\"     \n"
        );
    }

    #[test]
    fn nested_braces_and_strings_inside_template() {
        let a = "val s = \"${listOf(1).map { \"}\" + \"/*\" }.size} // x\"\n";
        assert_eq!(mask_comments(a), a);
        // Deeper nesting: string in template in string in template.
        let a = "val s = \"${\"${\"//\"}\"}\" // c\n";
        assert_eq!(mask_comments(a), "val s = \"${\"${\"//\"}\"}\"     \n");
    }

    #[test]
    fn raw_string_templates_and_escapes() {
        let a = "val s = \"\"\"a ${\"\"\"//x\"\"\"} ${m[\"k\"]} // y\"\"\" // c\n";
        assert_eq!(
            mask_comments(a),
            "val s = \"\"\"a ${\"\"\"//x\"\"\"} ${m[\"k\"]} // y\"\"\"     \n"
        );
        // Raw strings have no escapes; `\` before the closing quotes is content.
        assert_eq!(
            mask_comments("val s = \"\"\"\\\"\"\" // c\n"),
            "val s = \"\"\"\\\"\"\"     \n"
        );
        // Escaped `\$` and `\"` do not open templates / close the string.
        let a = "val s = \"\\${ \\\" // x\" // c\n";
        assert_eq!(mask_comments(a), "val s = \"\\${ \\\" // x\"     \n");
        // Character literals containing quotes.
        let a = "val c = '\"' // c\nval d = '\\'' // d\n";
        assert_eq!(mask_comments(a), "val c = '\"'     \nval d = '\\''     \n");
    }

    #[test]
    fn braces_inside_template_strings_do_not_close_blocks() {
        // `"${"}"}"` holds a `}` inside a nested string; the block must not end there.
        let f = facts("fun f(): String {\n  val s = \"${\"}\"}\"\n  return s\n}\nfun g() = 1\n");
        assert!(
            f.iter()
                .any(|x| x.0 == "return_statement" && x.1 == "return s"),
            "{f:?}"
        );
        assert_eq!(
            f.iter().filter(|x| x.0 == "function_declaration").count(),
            2,
            "{f:?}"
        );
        // Braces inside a multi-line raw string are not structural either.
        let f =
            facts("fun f(): String {\n  val s = \"\"\"\n}\n\"\"\"\n  return s\n}\nfun g() = 1\n");
        let decl = f
            .iter()
            .find(|x| x.0 == "function_declaration" && x.1 == "f")
            .unwrap();
        assert_eq!(decl.4, 5, "{f:?}");
        assert_eq!(
            f.iter().filter(|x| x.0 == "function_declaration").count(),
            2
        );
    }

    #[test]
    fn closing_row_template_with_nested_string_is_retained() {
        let o = payload("fun f(): String {\n  return \"${\"}\"}\" }\n");
        let n = payload("fun f(): String {\n  return \"${\"{\"}\" }\n");
        assert!(o.iter().any(|x| x.1 == "return \"${\"}\"}\""), "{o:?}");
        assert_ne!(o, n);
    }

    // ---- PR #6 review: structural braces on the closing row ----

    #[test]
    fn joined_closing_braces_add_no_statement() {
        let split = "class A {\n  fun f() {\n    println(1)\n  }\n}\n";
        let joined = "class A {\n  fun f() {\n    println(1)\n  } }\n";
        assert_eq!(payload(split), payload(joined));
        assert!(!payload(joined).iter().any(|x| x.1 == "}"));
        // A pure closing-brace row inside the body is structural too.
        let split = "class A {\n  fun f() {\n    if (x) {\n      g()\n    }\n  }\n  val y = 1\n}\n";
        let joined = "class A {\n  fun f() {\n    if (x) {\n      g()\n    } }\n  val y = 1\n}\n";
        assert_eq!(payload(split), payload(joined));
        // Closing-row body tokens are still retained alongside joined braces.
        let p = payload("class A {\n  fun f() {\n    g() } }\n");
        assert!(p.iter().any(|x| x.1 == "g() }"), "{p:?}");
    }

    #[test]
    fn truncated_literals_and_templates_never_panic() {
        let src = "class A {\n  val s = \"a ${m[\"k\"]} \\${ \\\" b\"\n  val r = \"\"\"x ${\"\"\"}\"\"\"} é \"\"\"\n  val c = '\\''\n  fun f() { g(\"${ {\"}\"} }\") } /* /* é */ */ }\n}\n";
        for (end, _) in src.char_indices() {
            let prefix = &src[..end];
            assert_eq!(mask_comments(prefix).len(), prefix.len());
            t::assert_valid_json(&process_impl(prefix), "truncated kotlin");
        }
    }

    #[test]
    fn output_is_deterministic() {
        let src = "// c\nclass A {\n  fun g() = 1 }\n";
        assert_eq!(process_impl(src), process_impl(src));
    }
}
