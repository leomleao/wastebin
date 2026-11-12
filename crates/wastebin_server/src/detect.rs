use serde::Serialize;

use crate::Highlighter;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DetectedLang {
    pub extension: String,
    pub name: String,
    pub is_code: bool,
}

/// Check whether `content` contains visual rich-editor HTML blocks (e.g. inline pasted assets, figures, tables, paragraphs).
pub fn has_rich_inline_html(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.starts_with("<!DOCTYPE html") || trimmed.starts_with("<html") {
        return false;
    }

    content.contains("<figure")
        || content.contains("<img ")
        || content.contains("class=\"inline-asset\"")
        || content.contains("class=\"inline-")
        || content.contains("<iframe")
        || content.contains("<table")
        || content.contains("<p>")
        || content.contains("<p ")
        || content.contains("<div><br></div>")
}

fn lang_name_for_ext(ext: &str) -> (&'static str, bool) {
    match ext.to_ascii_lowercase().as_str() {
        "rich" => ("Document", false),
        "txt" | "text" => ("Plain Text", false),
        "js" | "mjs" | "cjs" => ("JavaScript", true),
        "ts" | "tsx" => ("TypeScript", true),
        "jsx" => ("JSX", true),
        "php" => ("PHP", true),
        "abap" => ("ABAP", true),
        "md" | "markdown" => ("Markdown", true),
        "py" | "python" => ("Python", true),
        "sql" => ("SQL", true),
        "json" => ("JSON", true),
        "html" | "htm" => ("HTML", true),
        "css" | "scss" | "less" => ("CSS", true),
        "sh" | "bash" | "zsh" => ("Bash", true),
        "rs" | "rust" => ("Rust", true),
        "yaml" | "yml" => ("YAML", true),
        "toml" => ("TOML", true),
        "xml" | "svg" => ("XML", true),
        "go" => ("Go", true),
        "java" => ("Java", true),
        "c" | "h" => ("C", true),
        "cpp" | "cc" | "cxx" | "hpp" => ("C++", true),
        "cs" => ("C#", true),
        "rb" => ("Ruby", true),
        "swift" => ("Swift", true),
        "kt" | "kts" => ("Kotlin", true),
        _ => ("Code", true),
    }
}

pub fn detect_language(content: &str, explicit_ext: Option<&str>) -> DetectedLang {
    if let Some(ext) = explicit_ext {
        let ext_trimmed = ext.trim();
        if ext_trimmed.eq_ignore_ascii_case("rich") {
            return DetectedLang {
                extension: "rich".to_string(),
                name: "Document".to_string(),
                is_code: false,
            };
        }
        if !ext_trimmed.is_empty() && !ext_trimmed.eq_ignore_ascii_case("auto") {
            let normalized = ext_trimmed.to_ascii_lowercase();
            let (name, default_is_code) = lang_name_for_ext(&normalized);
            let is_code = default_is_code && !has_rich_inline_html(content);
            return DetectedLang {
                extension: normalized,
                name: name.to_string(),
                is_code,
            };
        }
    }

    if has_rich_inline_html(content) {
        return DetectedLang {
            extension: "rich".to_string(),
            name: "Document".to_string(),
            is_code: false,
        };
    }

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return DetectedLang {
            extension: "txt".to_string(),
            name: "Plain Text".to_string(),
            is_code: false,
        };
    }

    // 1. JSON check (strict valid JSON object or array)
    if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']')))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
    {
        return DetectedLang {
            extension: "json".to_string(),
            name: "JSON".to_string(),
            is_code: true,
        };
    }

    let upper = content.to_ascii_uppercase();

    // Score each candidate language
    let mut scores: Vec<(&str, &str, i32)> = Vec::new();

    // ABAP
    let mut abap_score = 0;
    for kw in [
        "REPORT ",
        "DATA:",
        "DATA(",
        "ENDSELECT",
        "LOOP AT ",
        "ENDLOOP",
        "WRITE:",
        "FORM ",
        "ENDFORM",
        "ENDCLASS",
        "ENDMETHOD",
        "FIELD-SYMBOLS",
        "TYPES:",
        "INTO TABLE ",
        "INTO CORRESPONDING FIELDS OF",
        "SELECT SINGLE ",
        "READ TABLE ",
        "SY-SUBRC",
    ] {
        if upper.contains(kw) {
            abap_score += 3;
        }
    }
    for kw in [
        "SELECT ", "CLASS ", "METHOD ", "APPEND ", "lt_", "ls_", "lv_",
    ] {
        if content.contains(kw) || upper.contains(kw) {
            abap_score += 1;
        }
    }
    scores.push(("abap", "ABAP", abap_score));

    // PHP
    let mut php_score = 0;
    if content.contains("<?php") || content.contains("<?=") {
        php_score += 10;
    }
    for kw in [
        "$_GET",
        "$_POST",
        "$_SERVER",
        "$this->",
        "public function ",
        "private function ",
        "protected function ",
        "namespace ",
        "echo ",
    ] {
        if content.contains(kw) {
            php_score += 2;
        }
    }
    scores.push(("php", "PHP", php_score));

    // TypeScript
    let mut ts_score = 0;
    for kw in [
        "interface ",
        "type ",
        ": string",
        ": number",
        ": boolean",
        ": any",
        "readonly ",
        "import type",
        "as const",
        "<T>",
        "<T,",
        "export interface ",
        "export type ",
    ] {
        if content.contains(kw) {
            ts_score += 3;
        }
    }
    for kw in ["const ", "let ", "function ", "=>", "export "] {
        if content.contains(kw) {
            ts_score += 1;
        }
    }
    scores.push(("ts", "TypeScript", ts_score));

    // JavaScript
    let mut js_score = 0;
    for kw in [
        "const ",
        "let ",
        "function ",
        "=>",
        "console.log",
        "document.",
        "window.",
        "export default",
        "module.exports",
        "require(",
        "async function",
        "await ",
    ] {
        if content.contains(kw) {
            js_score += 2;
        }
    }
    scores.push(("js", "JavaScript", js_score));

    // Rust
    let mut rs_score = 0;
    for kw in [
        "fn ",
        "let mut ",
        "pub struct ",
        "pub enum ",
        "pub fn ",
        "impl ",
        "println!(",
        "use std::",
        "match ",
        "#[derive(",
        "Some(",
        "Ok(",
        "Err(",
    ] {
        if content.contains(kw) {
            rs_score += 3;
        }
    }
    scores.push(("rs", "Rust", rs_score));

    // Python
    let mut py_score = 0;
    for kw in [
        "def ",
        "elif ",
        "if __name__",
        "from ",
        "self.",
        "print(",
        "import ",
        "None",
        "True",
        "False",
    ] {
        if content.contains(kw) {
            py_score += 2;
        }
    }
    scores.push(("py", "Python", py_score));

    // SQL
    let mut sql_score = 0;
    for kw in [
        "SELECT ",
        "FROM ",
        "WHERE ",
        "JOIN ",
        "GROUP BY",
        "ORDER BY",
        "INSERT INTO",
        "CREATE TABLE",
        "ALTER TABLE",
        "UPDATE ",
        "DELETE FROM",
    ] {
        if upper.contains(kw) {
            sql_score += 2;
        }
    }
    scores.push(("sql", "SQL", sql_score));

    // Bash / Shell
    let mut sh_score = 0;
    if trimmed.starts_with("#!/bin/bash")
        || trimmed.starts_with("#!/usr/bin/env bash")
        || trimmed.starts_with("#!/bin/sh")
    {
        sh_score += 10;
    }
    for kw in [
        "set -e",
        "echo $",
        "sudo ",
        "docker compose",
        "export ",
        "fi\n",
        "done\n",
    ] {
        if content.contains(kw) {
            sh_score += 2;
        }
    }
    scores.push(("sh", "Bash", sh_score));

    // HTML
    let mut html_score = 0;
    if upper.contains("<!DOCTYPE HTML") || content.contains("<html") {
        html_score += 10;
    }
    for kw in ["<head>", "<body>", "</div>", "<script", "<style"] {
        if content.contains(kw) {
            html_score += 2;
        }
    }
    scores.push(("html", "HTML", html_score));

    // CSS
    let mut css_score = 0;
    for kw in [
        "@media",
        "display:",
        "background-color:",
        "flex-direction:",
        "margin:",
        "padding:",
        "font-size:",
    ] {
        if content.contains(kw) {
            css_score += 2;
        }
    }
    scores.push(("css", "CSS", css_score));

    // Markdown
    let mut md_score = 0;
    for line in content.lines() {
        let l = line.trim_start();
        if l.starts_with("# ")
            || l.starts_with("## ")
            || l.starts_with("### ")
            || l.starts_with("```")
            || l.starts_with("- [ ]")
            || l.starts_with("- [x]")
            || (l.starts_with('|') && l.ends_with('|') && l.contains("---"))
        {
            md_score += 3;
        }
    }
    if content.contains("](") {
        md_score += 2;
    }
    scores.push(("md", "Markdown", md_score));

    // YAML
    let mut yaml_score = 0;
    for kw in ["services:", "version:", "apiVersion:", "kind:", "metadata:"] {
        if content.contains(kw) {
            yaml_score += 3;
        }
    }
    if !content.contains('{') && !content.contains(';') {
        let kv_lines = content
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty()
                    && !t.starts_with('#')
                    && t.contains(": ")
                    && t.split_once(": ").is_some_and(|(k, _)| {
                        k.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    })
            })
            .count();
        if kv_lines >= 3 {
            yaml_score += 3;
        }
    }
    scores.push(("yaml", "YAML", yaml_score));

    scores.sort_by_key(|score| std::cmp::Reverse(score.2));
    if let Some(&(ext, name, score)) = scores.first()
        && score >= 3
    {
        return DetectedLang {
            extension: ext.to_string(),
            name: name.to_string(),
            is_code: true,
        };
    }

    DetectedLang {
        extension: "txt".to_string(),
        name: "Plain Text".to_string(),
        is_code: false,
    }
}

/// Whether a pad with extension `ext` and content `text` was written in document mode. Older
/// pads stored document markup under the `html` extension.
pub fn is_document(ext: Option<&str>, text: &str) -> bool {
    match ext {
        Some(ext) if ext.eq_ignore_ascii_case("rich") => true,
        Some(ext) if ext.eq_ignore_ascii_case("html") => has_rich_inline_html(text),
        _ => false,
    }
}

/// Highlight `text` using `highlighter` (falling back to `"sql"` syntax for `"abap"` if syntect has
/// no built-in ABAP syntax). The result is not cached because pads change on every keystroke.
pub async fn highlight_code(highlighter: Highlighter, text: String, ext: String) -> Option<String> {
    if text.is_empty() {
        return None;
    }

    let syntax_ext = if ext.eq_ignore_ascii_case("abap") {
        "sql".to_string()
    } else {
        ext
    };

    let html = tokio::task::spawn_blocking(move || highlighter.highlight(text, Some(syntax_ext)))
        .await
        .ok()?
        .ok()?;

    Some(html.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_abap() {
        let code = "REPORT z_test_program.\nDATA: lt_mara TYPE TABLE OF mara,\n      ls_mara TYPE mara.\nSELECT * FROM mara INTO TABLE lt_mara.\nLOOP AT lt_mara INTO ls_mara.\n  WRITE: / ls_mara-matnr.\nENDLOOP.";
        let detected = detect_language(code, None);
        assert_eq!(detected.extension, "abap");
        assert_eq!(detected.name, "ABAP");
        assert!(detected.is_code);
    }

    #[test]
    fn detects_typescript() {
        let code = "export interface User {\n  readonly id: string;\n  count: number;\n}\nconst u: User = { id: '1', count: 42 };";
        let detected = detect_language(code, Some("auto"));
        assert_eq!(detected.extension, "ts");
        assert_eq!(detected.name, "TypeScript");
        assert!(detected.is_code);
    }

    #[test]
    fn detects_javascript() {
        let code = "const items = [1, 2, 3];\nfunction logAll() {\n  console.log(items.map(x => x * 2));\n}\nexport default logAll;";
        let detected = detect_language(code, None);
        assert_eq!(detected.extension, "js");
        assert_eq!(detected.name, "JavaScript");
        assert!(detected.is_code);
    }

    #[test]
    fn detects_php() {
        let code =
            "<?php\nnamespace App\\Controller;\npublic function index() {\n  echo $_GET['id'];\n}";
        let detected = detect_language(code, None);
        assert_eq!(detected.extension, "php");
        assert_eq!(detected.name, "PHP");
        assert!(detected.is_code);
    }

    #[test]
    fn detects_markdown() {
        let code = "# Project Roadmap\n\n## Milestones\n- [x] Initial release\n- [ ] Live sync\n";
        let detected = detect_language(code, None);
        assert_eq!(detected.extension, "md");
        assert_eq!(detected.name, "Markdown");
        assert!(detected.is_code);
    }

    #[test]
    fn detects_rich_html() {
        let doc = "<p>Here is an inline screenshot:</p><figure class=\"inline-asset\"><img src=\"/raw/abc.png\"></figure>";
        let detected = detect_language(doc, None);
        assert_eq!(detected.extension, "rich");
        assert_eq!(detected.name, "Document");
        assert!(!detected.is_code);
    }
}
