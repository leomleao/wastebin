//! Conversion between document pads (the HTML of the visual editor) and Markdown.
//!
//! The editor is line based and `white-space: pre-wrap`: a line is a `<div>` or a `<br>`, an
//! uploaded image or PDF is a `<figure class="inline-asset">` block. In Markdown a line break is a
//! newline and an asset is `![name](/raw/ID.ext)` (a PDF: `[name](/raw/ID.pdf)`) on its own line.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;

use ammonia::{Builder, UrlRelative};
use htmd::element_handler::{HandlerResult, Handlers};
use htmd::options::BulletListMarker;
use htmd::{Element, HtmlToMarkdown};
use markup5ever_rcdom::{Node, NodeData};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};

/// Convert the HTML of a document pad (`rich` extension) to the Markdown agents read and write.
///
/// Together with [`markdown_to_editor_html`] this is the seam between document pads and Markdown:
/// the MCP server converts with these two functions only.
#[must_use]
pub fn editor_html_to_markdown(html: &str) -> String {
    let markdown = converter().convert(html).unwrap_or_default();
    unescape_intraword_underscores(&markdown)
}

/// Convert Markdown written by an agent to sanitized HTML the pad editor accepts as a document
/// pad (`rich` extension). The inverse of [`editor_html_to_markdown`].
#[must_use]
pub fn markdown_to_editor_html(markdown: &str) -> String {
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    );
    let mut raw = String::with_capacity(markdown.len());
    html::push_html(&mut raw, editor_events(parser.collect()).into_iter());
    strip_layout_newlines(&sanitizer().clean(&raw).to_string())
}

// == Markdown to HTML

/// An uploaded file served at `/raw/{path}`, matching `isAssetUrl` in the editor.
struct Asset<'a> {
    path: &'a str,
}

impl<'a> Asset<'a> {
    fn parse(url: &'a str) -> Option<Self> {
        let path = url.strip_prefix("/raw/")?;
        let (id, ext) = path.split_once('.').unwrap_or((path, "a"));
        let id_ok = id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'));
        let ext_ok = ext.chars().all(|c| c.is_ascii_alphanumeric());
        (!id.is_empty() && id_ok && !ext.is_empty() && ext_ok).then_some(Self { path })
    }

    fn is_pdf(&self) -> bool {
        self.path.ends_with(".pdf")
    }

    /// The editor's figure markup (`figureHtml` in the editor), without the file size.
    fn figure(&self, name: &str) -> String {
        let name = escape_html(if name.trim().is_empty() {
            self.path
        } else {
            name
        });
        let path = self.path;
        if self.is_pdf() {
            format!(
                "<figure class=\"inline-asset\" data-kind=\"pdf\" data-src=\"/raw/{path}\">\
                 <figcaption>{name} · <a href=\"/raw/{path}\">open</a> · \
                 <a href=\"/dl/{path}\">download</a></figcaption></figure>"
            )
        } else {
            format!(
                "<figure class=\"inline-asset\" data-kind=\"image\">\
                 <img src=\"/raw/{path}\" alt=\"{name}\">\
                 <figcaption>{name} · <a href=\"/dl/{path}\">download</a></figcaption></figure>"
            )
        }
    }
}

fn editor_events(events: Vec<Event<'_>>) -> Vec<Event<'_>> {
    let mut out = Vec::with_capacity(events.len());
    // For each open image: whether it was turned into a link.
    let mut images = Vec::new();
    let mut i = 0;

    while i < events.len() {
        if let Some((figures, end)) = asset_paragraph(&events[i..]) {
            out.push(Event::Html(figures.into()));
            i += end + 1;
            continue;
        }

        match &events[i] {
            // The editor only shows this server's images: link to any other.
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let is_asset = Asset::parse(dest_url).is_some();
                images.push(!is_asset);
                out.push(if is_asset {
                    events[i].clone()
                } else {
                    Event::Start(Tag::Link {
                        link_type: *link_type,
                        dest_url: dest_url.clone(),
                        title: title.clone(),
                        id: id.clone(),
                    })
                });
            }
            Event::End(TagEnd::Image) => {
                out.push(if images.pop().unwrap_or(false) {
                    Event::End(TagEnd::Link)
                } else {
                    Event::End(TagEnd::Image)
                });
            }
            // A newline is a line break in the editor.
            Event::SoftBreak | Event::HardBreak => out.push(Event::InlineHtml("<br>".into())),
            other => out.push(other.clone()),
        }
        i += 1;
    }

    out
}

/// If `events` starts with a paragraph holding only uploaded images and PDF links, return their
/// figure markup and the index of the paragraph's end.
fn asset_paragraph(events: &[Event<'_>]) -> Option<(String, usize)> {
    if !matches!(events.first()?, Event::Start(Tag::Paragraph)) {
        return None;
    }

    let mut figures = String::new();
    let mut i = 1;
    loop {
        match events.get(i)? {
            Event::End(TagEnd::Paragraph) => return (!figures.is_empty()).then_some((figures, i)),
            Event::SoftBreak | Event::HardBreak => i += 1,
            Event::Text(text) if text.trim().is_empty() => i += 1,
            Event::Start(Tag::Image { dest_url, .. }) => {
                let asset = Asset::parse(dest_url)?;
                let (name, end) = inner_text(&events[i..])?;
                figures.push_str(&asset.figure(&name));
                i += end + 1;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let asset = Asset::parse(dest_url).filter(Asset::is_pdf)?;
                let (name, end) = inner_text(&events[i..])?;
                figures.push_str(&asset.figure(&name));
                i += end + 1;
            }
            _ => return None,
        }
    }
}

/// Plain text inside the tag `events` starts with, and the index of its end.
fn inner_text(events: &[Event<'_>]) -> Option<(String, usize)> {
    let mut text = String::new();
    let mut depth = 0usize;
    for (i, event) in events.iter().enumerate() {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return Some((text, i));
                }
            }
            Event::Text(t) | Event::Code(t) => text.push_str(t),
            _ => {}
        }
    }
    None
}

fn escape_html(text: &str) -> Cow<'_, str> {
    if !text.contains(['&', '<', '>', '"', '\'']) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Sanitizer with the editor's allowlist (`sanitizeHtml` in the editor).
fn sanitizer() -> &'static Builder<'static> {
    static CLEANER: OnceLock<Builder<'static>> = OnceLock::new();
    CLEANER.get_or_init(|| {
        let mut builder = Builder::empty();
        builder
            .tags(HashSet::from([
                "h1",
                "h2",
                "h3",
                "h4",
                "h5",
                "h6",
                "p",
                "div",
                "br",
                "hr",
                "ul",
                "ol",
                "li",
                "table",
                "thead",
                "tbody",
                "tfoot",
                "tr",
                "th",
                "td",
                "strong",
                "em",
                "u",
                "s",
                "del",
                "sub",
                "sup",
                "code",
                "pre",
                "blockquote",
                "a",
                "img",
                "figure",
                "figcaption",
            ]))
            .clean_content_tags(HashSet::from(["script", "style"]))
            .tag_attributes(HashMap::from([
                ("a", HashSet::from(["href"])),
                ("img", HashSet::from(["src", "alt"])),
                ("figure", HashSet::from(["data-kind", "data-src"])),
            ]))
            .allowed_classes(HashMap::from([("figure", HashSet::from(["inline-asset"]))]))
            .url_schemes(HashSet::from(["http", "https", "mailto"]))
            .url_relative(UrlRelative::PassThrough)
            .link_rel(None)
            .attribute_filter(|element, attribute, value| {
                let keep = match (element, attribute) {
                    ("img", "src") | ("figure", "data-src") => Asset::parse(value).is_some(),
                    ("figure", "data-kind") => matches!(value, "image" | "pdf"),
                    ("a", "href") => !value.starts_with("//"),
                    _ => true,
                };
                keep.then_some(Cow::Borrowed(value))
            });
        builder
    })
}

/// The editor shows every newline (`pre-wrap`): drop the ones between tags, keep code blocks.
fn strip_layout_newlines(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<pre>") {
        let (before, pre) = rest.split_at(start);
        push_without_newlines(&mut out, before);
        let end = pre
            .find("</pre>")
            .map_or(pre.len(), |end| end + "</pre>".len());
        out.push_str(&pre[..end]);
        rest = &pre[end..];
    }
    push_without_newlines(&mut out, rest);
    out
}

fn push_without_newlines(out: &mut String, html: &str) {
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\n' {
            out.push(c);
        } else if !out.ends_with('>') && chars.peek().is_some_and(|&next| next != '<') {
            out.push(' ');
        }
    }
}

// == HTML to Markdown

fn converter() -> &'static HtmlToMarkdown {
    static CONVERTER: OnceLock<HtmlToMarkdown> = OnceLock::new();
    CONVERTER.get_or_init(|| {
        HtmlToMarkdown::builder()
            .options(htmd::options::Options {
                bullet_list_marker: BulletListMarker::Dash,
                ul_bullet_spacing: 1,
                ol_number_spacing: 1,
                ..Default::default()
            })
            .add_handler(vec!["figure"], figure_handler)
            .add_handler(vec!["div"], line_handler)
            .add_handler(vec!["br"], |_: &dyn Handlers, _: Element| Some("\n".into()))
            .add_handler(vec!["hr"], |_: &dyn Handlers, _: Element| {
                Some("\n\n---\n\n".into())
            })
            .add_handler(vec!["s", "del", "strike"], strikethrough_handler)
            .build()
    })
}

/// An uploaded image or PDF becomes a Markdown image or link; its caption is left out.
fn figure_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let markdown = asset_markdown(&element);
    match markdown {
        Some(markdown) => Some(format!("\n\n{markdown}\n\n").into()),
        None => handlers.fallback(element),
    }
}

fn asset_markdown(element: &Element) -> Option<String> {
    let attrs: HashMap<&str, &str> = element
        .attrs
        .iter()
        .map(|attr| (&*attr.name.local, &*attr.value))
        .collect();
    if !attrs
        .get("class")?
        .split_whitespace()
        .any(|class| class == "inline-asset")
    {
        return None;
    }

    // The caption reads `name · size · links`.
    let caption = find_element(element.node, "figcaption").map(|node| text_content(&node));
    let caption_name = caption
        .as_deref()
        .and_then(|caption| caption.split(" · ").next())
        .unwrap_or_default()
        .trim();

    if attrs.get("data-kind") == Some(&"pdf") {
        let src = attrs.get("data-src")?;
        Asset::parse(src)?;
        return Some(format!("[{}]({src})", escape_markdown(caption_name)));
    }

    let img = find_element(element.node, "img")?;
    let NodeData::Element { attrs, .. } = &img.data else {
        return None;
    };
    let attrs = attrs.borrow();
    let attr = |name: &str| {
        attrs
            .iter()
            .find(|attr| &*attr.name.local == name)
            .map(|attr| attr.value.to_string())
    };
    let src = attr("src")?;
    Asset::parse(&src)?;
    let name = attr("alt")
        .filter(|alt| !alt.trim().is_empty())
        .unwrap_or_else(|| caption_name.to_string());
    Some(format!("![{}]({src})", escape_markdown(&name)))
}

/// A `<div>` is one line of the editor; an empty one is a blank line. Only the line's leading
/// newline is emitted: htmd adds up the newlines between siblings. Leading spaces become no-break
/// spaces, which Markdown keeps, so that an indented line stays indented.
fn line_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let content = handlers.walk_children(element.node).content;
    let content = content.trim_matches('\n');
    let has_blocks = element.node.children.borrow().iter().any(|child| {
        matches!(&child.data, NodeData::Element { name, .. }
            if !matches!(&*name.local, "br" | "a" | "strong" | "b" | "em" | "i" | "u" | "s"
                | "del" | "strike" | "sub" | "sup" | "code" | "span" | "img"))
    });
    Some(if content.is_empty() {
        "\n\n".into()
    } else if has_blocks {
        format!("\n\n{content}\n\n").into()
    } else {
        let content = content.trim_start_matches(' ');
        format!("\n{}{content}", indentation(element.node)).into()
    })
}

/// The leading spaces of `node`'s text as no-break spaces, a tab as four.
fn indentation(node: &Rc<Node>) -> String {
    let children = node.children.borrow();
    let Some(NodeData::Text { contents }) = children.first().map(|child| &child.data) else {
        return String::new();
    };
    contents
        .borrow()
        .chars()
        .map_while(|c| match c {
            ' ' => Some("\u{a0}"),
            '\t' => Some("\u{a0}\u{a0}\u{a0}\u{a0}"),
            _ => None,
        })
        .collect()
}

fn strikethrough_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let content = handlers.walk_children(element.node).content;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Some(content.into());
    }
    let start = content.len() - content.trim_start().len();
    let end = content.trim_end().len();
    Some(format!("{}~~{trimmed}~~{}", &content[..start], &content[end..]).into())
}

/// The first descendant element named `tag`.
fn find_element(node: &Rc<Node>, tag: &str) -> Option<Rc<Node>> {
    node.children
        .borrow()
        .iter()
        .find_map(|child| match &child.data {
            NodeData::Element { name, .. } if &*name.local == tag => Some(Rc::clone(child)),
            _ => find_element(child, tag),
        })
}

fn text_content(node: &Rc<Node>) -> String {
    let mut text = String::new();
    for child in node.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } => text.push_str(&contents.borrow()),
            _ => text.push_str(&text_content(child)),
        }
    }
    text
}

fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '[' | ']' | '*' | '_' | '`' | '<') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// htmd escapes every `_`, but one between two alphanumerics can never start or end emphasis:
/// unescape those so identifiers and file names stay readable. Code is left as is.
fn unescape_intraword_underscores(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut fence: Option<usize> = None;

    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let ticks = trimmed.len() - trimmed.trim_start_matches('`').len();
        match fence {
            Some(open) => {
                if ticks >= open && trimmed[ticks..].trim().is_empty() {
                    fence = None;
                }
                out.push_str(line);
            }
            None if ticks >= 3 => {
                fence = Some(ticks);
                out.push_str(line);
            }
            None => unescape_line(line, &mut out),
        }
    }

    out
}

fn unescape_line(line: &str, out: &mut String) {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        match c {
            '\\' if i + 1 < chars.len() => {
                let intraword = chars[i + 1] == '_'
                    && i > 0
                    && chars[i - 1].is_alphanumeric()
                    && chars.get(i + 2).is_some_and(|next| next.is_alphanumeric());
                if !intraword {
                    out.push('\\');
                }
                out.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                let run = chars[i..].iter().take_while(|&&c| c == '`').count();
                let close = (i + run..chars.len()).find(|&j| {
                    chars[j..].iter().take_while(|&&c| c == '`').count() == run
                        && chars[j - 1] != '`'
                });
                let end = close.map_or(i + run, |j| j + run);
                out.extend(&chars[i..end]);
                i = end;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Markdown that converts to HTML and back unchanged.
    fn assert_round_trip(markdown: &str) {
        let html = markdown_to_editor_html(markdown);
        assert_eq!(editor_html_to_markdown(&html), markdown, "html: {html}");
    }

    /// Markdown that converts to `html`, and back unchanged.
    fn assert_converts(markdown: &str, html: &str) {
        assert_eq!(markdown_to_editor_html(markdown), html);
        assert_eq!(editor_html_to_markdown(html), markdown);
    }

    #[test]
    fn headings_and_inline_formatting() {
        assert_converts(
            "# Title\n\n### Small\n\n**bold**, *italic*, ~~gone~~, `code` and [a link](https://example.com).",
            "<h1>Title</h1><h3>Small</h3><p><strong>bold</strong>, <em>italic</em>, <del>gone</del>, \
             <code>code</code> and <a href=\"https://example.com\">a link</a>.</p>",
        );
    }

    #[test]
    fn lists() {
        assert_converts(
            "- one\n- two\n  - nested\n    - deeper\n- three",
            "<ul><li>one</li><li>two<ul><li>nested<ul><li>deeper</li></ul></li></ul></li><li>three</li></ul>",
        );
        assert_round_trip("1. one\n2. two\n   1. nested\n   2. again\n3. three");
        assert_round_trip("- mixed\n  1. ordered\n  2. inside");
    }

    #[test]
    fn code_block_keeps_its_newlines() {
        let html =
            markdown_to_editor_html("Before\n\n```\nfn main() {\n    run();\n}\n```\n\nAfter");
        assert_eq!(
            html,
            "<p>Before</p><pre><code>fn main() {\n    run();\n}\n</code></pre><p>After</p>"
        );
        assert_round_trip("Before\n\n```\nfn main() {\n    run();\n}\n```\n\nAfter");
    }

    #[test]
    fn code_block_language_is_dropped() {
        // The editor keeps no attributes on `<code>`.
        let html = markdown_to_editor_html("```rust\nlet x = 1;\n```");
        assert_eq!(editor_html_to_markdown(&html), "```\nlet x = 1;\n```");
    }

    #[test]
    fn table() {
        assert_converts(
            "| a | b |\n| - | - |\n| 1 | 2 |",
            "<table><thead><tr><th>a</th><th>b</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table>",
        );
    }

    #[test]
    fn quote_and_rule() {
        assert_converts(
            "> quoted\n\n---\n\nafter",
            "<blockquote><p>quoted</p></blockquote><hr><p>after</p>",
        );
    }

    #[test]
    fn newlines_are_editor_lines() {
        assert_converts(
            "line one\nline two\n\nnext paragraph",
            "<p>line one<br>line two</p><p>next paragraph</p>",
        );
    }

    #[test]
    fn editor_lines_become_markdown_lines() {
        let html = "first<div>second</div><div><br></div><div>third</div><div>fourth</div>";
        assert_eq!(
            editor_html_to_markdown(html),
            "first\nsecond\n\nthird\nfourth"
        );
    }

    #[test]
    fn indented_editor_lines_stay_indented() {
        // A code pad turned document by an upload: one line per `<div>`.
        let html = "<div>fn main() {</div><div>    run();</div><div>\tdone();</div><div>}</div>";
        let markdown = editor_html_to_markdown(html);
        let four = "\u{a0}".repeat(4);
        assert_eq!(markdown, format!("fn main() {{\n{four}run();\n{four}done();\n}}"));

        // A no-op edit keeps the indentation.
        let html = markdown_to_editor_html(&markdown);
        let four = "&nbsp;".repeat(4);
        assert_eq!(
            html,
            format!("<p>fn main() {{<br>{four}run();<br>{four}done();<br>}}</p>")
        );
        assert_eq!(editor_html_to_markdown(&html), markdown);
    }

    #[test]
    fn image_becomes_figure() {
        assert_converts(
            "![shot_1.png](/raw/abc.png)",
            "<figure class=\"inline-asset\" data-kind=\"image\"><img src=\"/raw/abc.png\" alt=\"shot_1.png\">\
             <figcaption>shot_1.png · <a href=\"/dl/abc.png\">download</a></figcaption></figure>",
        );
    }

    #[test]
    fn pdf_becomes_figure() {
        assert_converts(
            "[spec.pdf](/raw/abc.pdf)",
            "<figure class=\"inline-asset\" data-kind=\"pdf\" data-src=\"/raw/abc.pdf\">\
             <figcaption>spec.pdf · <a href=\"/raw/abc.pdf\">open</a> · \
             <a href=\"/dl/abc.pdf\">download</a></figcaption></figure>",
        );
    }

    #[test]
    fn figures_between_text() {
        assert_round_trip(
            "Intro\n\n![a.png](/raw/a.png)\n\n![b.png](/raw/b.png)\n\n[c.pdf](/raw/c.pdf)\n\nOutro",
        );
        // Assets on consecutive lines are one paragraph in Markdown, separate figures here.
        let html = markdown_to_editor_html("![a.png](/raw/a.png)\n![b.png](/raw/b.png)");
        assert_eq!(html.matches("<figure").count(), 2, "{html}");
        assert!(!html.contains("<p>"), "{html}");
    }

    #[test]
    fn figure_without_name_uses_the_file() {
        let html = markdown_to_editor_html("![](/raw/abc.png)");
        assert!(html.contains("alt=\"abc.png\""), "{html}");
        assert_eq!(editor_html_to_markdown(&html), "![abc.png](/raw/abc.png)");
    }

    #[test]
    fn figure_name_is_escaped() {
        assert_round_trip("![a \\[draft\\] \\*v2\\*.png](/raw/abc.png)");
        let html = markdown_to_editor_html("![<b>x</b>\"y](/raw/abc.png)");
        assert!(html.contains("alt=\"x&quot;y\""), "{html}");
    }

    #[test]
    fn editor_figures_drop_caption_text() {
        // As saved by the editor in German, with the size; and with leaked editor-only controls.
        let html = "<p>Before</p>\
            <figure class=\"inline-asset\" data-kind=\"image\"><img src=\"/raw/abc.png\" alt=\"shot.png\">\
            <figcaption>shot.png · 12.3 KiB · <a href=\"/dl/abc.png\">herunterladen</a>\
            <span class=\"editor-only\"> · <button class=\"asset-remove\">entfernen</button></span>\
            </figcaption></figure>\
            <figure class=\"inline-asset\" data-kind=\"pdf\" data-src=\"/raw/def.pdf\">\
            <figcaption>my_report.pdf · 1.2 MiB · <a href=\"/raw/def.pdf\">öffnen</a> · \
            <a href=\"/dl/def.pdf\">herunterladen</a></figcaption></figure>\
            <div>After</div>";
        assert_eq!(
            editor_html_to_markdown(html),
            "Before\n\n![shot.png](/raw/abc.png)\n\n[my_report.pdf](/raw/def.pdf)\n\nAfter"
        );
    }

    #[test]
    fn inline_assets_stay_inline() {
        assert_converts(
            "see ![icon](/raw/abc.png) and [the spec](/raw/def.pdf) here",
            "<p>see <img src=\"/raw/abc.png\" alt=\"icon\"> and <a href=\"/raw/def.pdf\">the spec</a> here</p>",
        );
    }

    #[test]
    fn foreign_images_become_links() {
        // The editor only shows this server's images.
        assert_eq!(
            markdown_to_editor_html("![logo](https://example.com/logo.png)"),
            "<p><a href=\"https://example.com/logo.png\">logo</a></p>"
        );
        assert_eq!(
            markdown_to_editor_html("![x](//example.com/x.png)"),
            "<p><a>x</a></p>"
        );
    }

    #[test]
    fn output_is_sanitized() {
        let html = markdown_to_editor_html(
            "<script>alert(1)</script>\n\n[x](javascript:alert(1)) <img src=x onerror=alert(1)>\n\n\
             <figure class=\"inline-asset evil\" data-kind=\"x\" data-src=\"javascript:x\" onclick=\"x\"></figure>\n\n\
             <span style=\"color:red\">kept</span> <iframe src=\"/raw/a.pdf\"></iframe>",
        );
        for bad in [
            "script",
            "alert",
            "javascript",
            "onerror",
            "onclick",
            "evil",
            "data-kind",
            "style",
            "iframe",
            "span",
        ] {
            assert!(!html.contains(bad), "{bad}: {html}");
        }
        assert!(html.contains("<figure class=\"inline-asset\">"), "{html}");
        assert!(html.contains("kept"), "{html}");
    }

    #[test]
    fn no_newlines_between_tags() {
        let html = markdown_to_editor_html(
            "# A\n\n- b\n- c\n\n| d |\n| - |\n| e |\n\n> f\n\n1. g\n\n   h\n2. i",
        );
        assert!(!html.contains('\n'), "{html}");
    }

    #[test]
    fn intraword_underscores_stay_unescaped() {
        assert_round_trip("WASTEBIN_MCP and my_file.txt, but \\_not\\_ this or `a\\_b`");
        assert_eq!(
            editor_html_to_markdown("<p>snake_case</p><pre><code>a\\_b</code></pre>"),
            "snake_case\n\n```\na\\_b\n```"
        );
    }

    #[test]
    fn task_lists_are_text() {
        // The editor has no checkboxes: the markers are kept as text, escaped.
        let markdown = editor_html_to_markdown(&markdown_to_editor_html("- [x] done\n- [ ] open"));
        assert_eq!(markdown, "- \\[x\\] done\n- \\[ \\] open");
        assert_round_trip(&markdown);
    }

    #[test]
    fn conversion_is_stable() {
        // Whatever Markdown comes in, one round trip reaches a fixed point.
        for markdown in [
            "Setext\n======\n\n* star list\n+ plus\n\n2) paren list\n\n***\n\n~~~\ntilde fence\n~~~",
            "Hard  \nbreak and trailing\\\nbackslash\n\n    indented code",
            "__strong__ _em_ ***both*** <kbd>raw</kbd> &amp; &lt;tag&gt; 1. not a list",
            "- loose\n\n- list\n\n  with paragraphs",
            "Text\n![a.png](/raw/a.png)\nmore text",
            "| left | right |\n|:-----|------:|\n| `x` | **y** |",
            "[^1] footnote\n\n[^1]: note",
        ] {
            let once = editor_html_to_markdown(&markdown_to_editor_html(markdown));
            let twice = editor_html_to_markdown(&markdown_to_editor_html(&once));
            assert_eq!(once, twice, "from: {markdown}");
            assert_eq!(
                markdown_to_editor_html(&once),
                markdown_to_editor_html(&twice)
            );
        }
    }

    #[test]
    fn empty() {
        assert_eq!(markdown_to_editor_html(""), "");
        assert_eq!(editor_html_to_markdown(""), "");
    }
}
