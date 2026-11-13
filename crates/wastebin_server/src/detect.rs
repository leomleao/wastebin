use crate::Highlighter;

/// Extension of the language `content` is written in. An explicit extension other than `auto` is
/// taken as is; documents (`rich`) are only ever chosen explicitly.
pub fn detect_language(content: &str, explicit_ext: Option<&str>) -> String {
    if let Some(ext) = explicit_ext.map(str::trim)
        && !ext.is_empty()
        && !ext.eq_ignore_ascii_case("auto")
    {
        return ext.to_ascii_lowercase();
    }

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return "txt".to_string();
    }

    // 1. JSON check (strict valid JSON object or array)
    if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']')))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
    {
        return "json".to_string();
    }

    let upper = content.to_ascii_uppercase();

    // Score each candidate language
    let mut scores: Vec<(&str, i32)> = Vec::new();

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
    scores.push(("abap", abap_score));

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
    scores.push(("php", php_score));

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
    scores.push(("ts", ts_score));

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
    scores.push(("js", js_score));

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
    scores.push(("rs", rs_score));

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
    scores.push(("py", py_score));

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
    scores.push(("sql", sql_score));

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
    scores.push(("sh", sh_score));

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
    scores.push(("html", html_score));

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
    scores.push(("css", css_score));

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
    scores.push(("md", md_score));

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
    scores.push(("yaml", yaml_score));

    scores.sort_by_key(|score| std::cmp::Reverse(score.1));
    if let Some(&(ext, score)) = scores.first()
        && score >= 3
    {
        return ext.to_string();
    }

    "txt".to_string()
}

/// Highlight `text` using `highlighter` (falling back to `"sql"` syntax for `"abap"` if syntect has
/// no built-in ABAP syntax). The result is not cached because pads change all the time.
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
        assert_eq!(detect_language(code, None), "abap");
    }

    #[test]
    fn detects_typescript() {
        let code = "export interface User {\n  readonly id: string;\n  count: number;\n}\nconst u: User = { id: '1', count: 42 };";
        assert_eq!(detect_language(code, Some("auto")), "ts");
    }

    #[test]
    fn detects_javascript() {
        let code = "const items = [1, 2, 3];\nfunction logAll() {\n  console.log(items.map(x => x * 2));\n}\nexport default logAll;";
        assert_eq!(detect_language(code, None), "js");
    }

    #[test]
    fn detects_php() {
        let code =
            "<?php\nnamespace App\\Controller;\npublic function index() {\n  echo $_GET['id'];\n}";
        assert_eq!(detect_language(code, None), "php");
    }

    #[test]
    fn detects_markdown() {
        let code = "# Project Roadmap\n\n## Milestones\n- [x] Initial release\n- [ ] Live sync\n";
        assert_eq!(detect_language(code, None), "md");
    }

    #[test]
    fn documents_are_only_explicit() {
        let doc = "<p>Here is an inline screenshot:</p><figure class=\"inline-asset\"><img src=\"/raw/abc.png\"></figure>";
        assert_ne!(detect_language(doc, None), "rich");
        assert_ne!(detect_language(doc, Some("auto")), "rich");
        assert_eq!(detect_language(doc, Some("rich")), "rich");
        assert_eq!(detect_language(doc, Some(" RS ")), "rs");
    }
}
