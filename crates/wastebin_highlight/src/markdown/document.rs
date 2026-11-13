//! Conversion between document pads (the HTML of the visual editor) and Markdown.
//!
//! The editor is line based and `white-space: pre-wrap`: a line is a `<div>` or a `<br>`, an
//! uploaded image or PDF is a `<figure class="inline-asset">` block. In Markdown a line break is a
//! newline and an asset is `![name](/raw/ID.ext)` (a PDF: `[name](/raw/ID.pdf)`) on its own line.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::sync::OnceLock;

use ammonia::{Builder, UrlRelative};
use htmd::element_handler::{HandlerResult, Handlers};
use htmd::options::BulletListMarker;
use htmd::{Element, HtmlToMarkdown};
use markup5ever_rcdom::{Node, NodeData};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

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
pub fn markdown_to_editor_html(markdown: &str, uploads: &Uploads) -> String {
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    );
    let events = indented_lines(markdown, parser.into_offset_iter().collect());
    let mut raw = String::with_capacity(markdown.len());
    html::push_html(&mut raw, editor_events(events, uploads).into_iter());
    strip_layout_newlines(&sanitizer().clean(&raw).to_string())
}

/// What [`markdown_to_editor_html`] knows about this server's uploads.
#[derive(Debug, Default)]
pub struct Uploads {
    /// Origin of this server (`https://bin.lan`). Links into it are made relative, so that the
    /// absolute URL of an upload makes a figure too.
    pub origin: Option<String>,
    /// Formatted sizes by path below `/raw/` (`abc.png` → `879 KB`), shown in figures.
    pub sizes: HashMap<String, String>,
}

/// The editor's markup for the upload at `/raw/{path}` named `name`, with its formatted `size`
/// if known (`figureHtml` in the editor).
#[must_use]
pub fn figure_html(path: &str, name: &str, size: Option<&str>) -> String {
    let name = escape_html(if name.trim().is_empty() { path } else { name });
    let size = size.map_or_else(String::new, |size| format!("{} · ", escape_html(size)));
    if path.ends_with(".pdf") {
        format!(
            "<figure class=\"inline-asset\" data-kind=\"pdf\" data-src=\"/raw/{path}\">\
             <figcaption>{name} · {size}<a href=\"/raw/{path}\">open</a> · \
             <a href=\"/dl/{path}\">download</a></figcaption></figure>"
        )
    } else {
        format!(
            "<figure class=\"inline-asset\" data-kind=\"image\">\
             <img src=\"/raw/{path}\" alt=\"{name}\">\
             <figcaption>{name} · {size}<a href=\"/dl/{path}\">download</a></figcaption></figure>"
        )
    }
}

/// The text of a code pad as the lines of a document pad, as the editor converts it.
#[must_use]
pub fn text_to_editor_html(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    text.split('\n')
        .map(|line| match line {
            "" => "<div><br></div>".to_string(),
            line => format!("<div>{}</div>", escape_html(line)),
        })
        .collect()
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

    fn figure(&self, name: &str, sizes: &HashMap<String, String>) -> String {
        figure_html(self.path, name, sizes.get(self.path).map(String::as_str))
    }
}

/// `events` of `markdown`, with the indentation Markdown drops from the continuation lines of a
/// top-level paragraph put back: the editor shows every line as it is.
fn indented_lines<'a>(markdown: &'a str, events: Vec<(Event<'a>, Range<usize>)>) -> Vec<Event<'a>> {
    let mut out = Vec::with_capacity(events.len());
    let mut depth = 0usize;
    for (i, (event, range)) in events.iter().enumerate() {
        match event {
            Event::Start(Tag::List(_) | Tag::BlockQuote(_)) => depth += 1,
            Event::End(TagEnd::List(_) | TagEnd::BlockQuote(_)) => depth -= 1,
            _ => {}
        }
        out.push(event.clone());
        if depth == 0
            && matches!(event, Event::SoftBreak | Event::HardBreak)
            && let Some((_, next)) = events.get(i + 1)
            && let Some(gap) = markdown.get(range.end..next.start)
            && !gap.is_empty()
            && gap.chars().all(|c| matches!(c, ' ' | '\t'))
        {
            out.push(Event::Text(gap.into()));
        }
    }
    out
}

fn editor_events<'a>(events: Vec<Event<'a>>, uploads: &Uploads) -> Vec<Event<'a>> {
    let events: Vec<Event<'a>> = events
        .into_iter()
        .map(|event| relative_link(event, uploads))
        .collect();
    let sizes = &uploads.sizes;
    let mut out = Vec::with_capacity(events.len());
    // For each open image: whether it was turned into a link.
    let mut images = Vec::new();
    let mut line_start = false;
    let mut i = 0;

    while i < events.len() {
        if let Some((figures, end)) = asset_paragraph(&events[i..], sizes) {
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
            // Indentation read as no-break spaces is stored as spaces again, which the editor
            // shows and copies as they are.
            Event::Text(text) if line_start => {
                let rest = text.trim_start_matches('\u{a0}');
                let indent = " ".repeat(text[..text.len() - rest.len()].chars().count());
                out.push(Event::Text(format!("{indent}{rest}").into()));
            }
            other => out.push(other.clone()),
        }
        line_start = matches!(
            events[i],
            Event::Start(Tag::Paragraph) | Event::SoftBreak | Event::HardBreak
        );
        i += 1;
    }

    out
}

/// `event` with the URL of a link or image into this server made relative.
fn relative_link<'a>(event: Event<'a>, uploads: &Uploads) -> Event<'a> {
    let relative = |url: CowStr<'a>| -> CowStr<'a> {
        match local_path(&url, uploads) {
            Some(path) => path.to_string().into(),
            None => url,
        }
    };
    match event {
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: relative(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: relative(dest_url),
            title,
            id,
        }),
        event => event,
    }
}

/// The path of `url` if it points into this server: an http(s) URL with the server's origin, or
/// of one of the known uploads, whatever its host.
fn local_path<'u>(url: &'u str, uploads: &Uploads) -> Option<&'u str> {
    let (scheme, rest) = url.split_once("://")?;
    if !["http", "https"]
        .iter()
        .any(|s| s.eq_ignore_ascii_case(scheme))
    {
        return None;
    }
    let path = &rest[rest.find(['/', '?', '#']).unwrap_or(rest.len())..];
    if !path.starts_with('/') || path.starts_with("//") {
        return None;
    }

    let origin = &url[..url.len() - path.len()];
    let same_origin = uploads
        .origin
        .as_deref()
        .is_some_and(|ours| normalized_origin(ours) == normalized_origin(origin));
    let id = |path: &str| path.split('.').next().unwrap_or_default().to_string();
    let known_upload = path
        .strip_prefix("/raw/")
        .is_some_and(|file| uploads.sizes.keys().any(|known| id(known) == id(file)));
    (same_origin || known_upload).then_some(path)
}

/// `origin` (`scheme://host[:port]`) in lower case, without the scheme's default port.
fn normalized_origin(origin: &str) -> String {
    let origin = origin.trim_end_matches('/').to_ascii_lowercase();
    for (scheme, port) in [("http://", ":80"), ("https://", ":443")] {
        if origin.starts_with(scheme)
            && let Some(origin) = origin.strip_suffix(port)
        {
            return origin.to_string();
        }
    }
    origin
}

/// If `events` starts with a paragraph holding only uploaded images and PDF links, return their
/// figure markup and the index of the paragraph's end.
fn asset_paragraph(
    events: &[Event<'_>],
    sizes: &HashMap<String, String>,
) -> Option<(String, usize)> {
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
                figures.push_str(&asset.figure(&name, sizes));
                i += end + 1;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let asset = Asset::parse(dest_url).filter(Asset::is_pdf)?;
                let (name, end) = inner_text(&events[i..])?;
                figures.push_str(&asset.figure(&name, sizes));
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
            .add_handler(vec!["p"], |handlers: &dyn Handlers, element: Element| {
                keep_indentation(element.node.children.borrow().first());
                handlers.fallback(element)
            })
            .add_handler(vec!["br"], |_: &dyn Handlers, element: Element| {
                keep_indentation(next_sibling(element.node).as_ref());
                Some("\n".into())
            })
            .add_handler(vec!["hr"], |_: &dyn Handlers, _: Element| {
                Some("\n\n---\n\n".into())
            })
            .add_handler(vec!["s", "del", "strike"], strikethrough_handler)
            .add_handler(vec!["u", "sub", "sup"], inline_html_handler)
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
/// newline is emitted: htmd adds up the newlines between siblings.
fn line_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    keep_indentation(element.node.children.borrow().first());
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
        format!("\n{content}").into()
    })
}

/// Turn the leading spaces of the line starting with `node`, if it is text outside a code block,
/// into no-break spaces (a tab into four), which htmd and Markdown keep. [`plain_indentation`]
/// turns them back into spaces where Markdown keeps those too.
fn keep_indentation(node: Option<&Rc<Node>>) {
    let Some(node) = node else { return };
    let NodeData::Text { contents } = &node.data else {
        return;
    };
    if ancestors(node).any(|ancestor| {
        matches!(&ancestor.data, NodeData::Element { name, .. } if &*name.local == "pre")
    }) {
        return;
    }
    let mut contents = contents.borrow_mut();
    let rest = contents.trim_start_matches([' ', '\t']);
    if rest.len() == contents.len() {
        return;
    }
    let indented: String = contents
        .chars()
        .map_while(|c| match c {
            ' ' => Some("\u{a0}"),
            '\t' => Some("\u{a0}\u{a0}\u{a0}\u{a0}"),
            _ => None,
        })
        .chain([rest])
        .collect();
    contents.clear();
    contents.push_slice(&indented);
}

fn parent(node: &Node) -> Option<Rc<Node>> {
    let weak = node.parent.take();
    let parent = weak.as_ref().and_then(Weak::upgrade);
    node.parent.set(weak);
    parent
}

fn ancestors(node: &Rc<Node>) -> impl Iterator<Item = Rc<Node>> {
    std::iter::successors(parent(node), |node| parent(node))
}

fn next_sibling(node: &Rc<Node>) -> Option<Rc<Node>> {
    let parent = parent(node)?;
    let children = parent.children.borrow();
    let index = children.iter().position(|child| Rc::ptr_eq(child, node))?;
    children.get(index + 1).cloned()
}

/// Markdown has no underline, subscript or superscript: they stay inline HTML.
fn inline_html_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let content = handlers.walk_children(element.node).content;
    Some(format!("<{tag}>{content}</{tag}>", tag = element.tag).into())
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
            None => {
                let previous = out.strip_suffix('\n').unwrap_or(&out);
                let continues = !previous
                    .rsplit('\n')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .is_empty();
                let line = if continues {
                    plain_indentation(line)
                } else {
                    line.into()
                };
                unescape_line(&line, &mut out);
            }
        }
    }

    out
}

/// `line`, continuing a paragraph, with its leading no-break spaces as spaces: Markdown keeps
/// the line in the paragraph, and the editor gets its indentation back. Not where the spaces
/// would let the line start a block.
fn plain_indentation(line: &str) -> Cow<'_, str> {
    let rest = line.trim_start_matches('\u{a0}');
    let starts_block = rest.starts_with(|c: char| {
        c.is_ascii_digit() || matches!(c, '-' | '+' | '*' | '>' | '#' | '=' | '|' | '`' | '~' | '<')
    });
    if rest.len() == line.len() || starts_block {
        return line.into();
    }
    let indent = " ".repeat(line[..line.len() - rest.len()].chars().count());
    format!("{indent}{rest}").into()
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

    fn to_html(markdown: &str) -> String {
        markdown_to_editor_html(markdown, &Uploads::default())
    }

    /// Markdown that converts to HTML and back unchanged.
    fn assert_round_trip(markdown: &str) {
        let html = to_html(markdown);
        assert_eq!(editor_html_to_markdown(&html), markdown, "html: {html}");
    }

    /// Markdown that converts to `html`, and back unchanged.
    fn assert_converts(markdown: &str, html: &str) {
        assert_eq!(to_html(markdown), html);
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
        let html = to_html("Before\n\n```\nfn main() {\n    run();\n}\n```\n\nAfter");
        assert_eq!(
            html,
            "<p>Before</p><pre><code>fn main() {\n    run();\n}\n</code></pre><p>After</p>"
        );
        assert_round_trip("Before\n\n```\nfn main() {\n    run();\n}\n```\n\nAfter");
    }

    #[test]
    fn code_block_language_is_dropped() {
        // The editor keeps no attributes on `<code>`.
        let html = to_html("```rust\nlet x = 1;\n```");
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
        assert_eq!(markdown, "fn main() {\n    run();\n    done();\n}");

        // A no-op edit keeps the indentation, as spaces.
        let html = to_html(&markdown);
        assert_eq!(html, "<p>fn main() {<br>    run();<br>    done();<br>}</p>");
        assert_eq!(editor_html_to_markdown(&html), markdown);
    }

    #[test]
    fn indentation_is_spaces_where_markdown_keeps_them() {
        // A paragraph's first line, or a line that would start a block, keeps no-break spaces.
        let nbsp = "\u{a0}\u{a0}";
        for (html, markdown) in [
            (
                "<p>  first<br>  second</p>",
                format!("{nbsp}first\n  second"),
            ),
            ("<div>a</div><div>  - b</div>", format!("a\n{nbsp}- b")),
            (
                "<div>a</div><div><br></div><div>  b</div>",
                format!("a\n\n{nbsp}b"),
            ),
            (
                "<pre><code>a\n  b\n</code></pre>",
                "```\na\n  b\n```".to_string(),
            ),
        ] {
            assert_eq!(editor_html_to_markdown(html), markdown, "{html}");
            // Stored as spaces, and read back the same.
            let stored = to_html(&markdown);
            assert!(!stored.contains("&nbsp;"), "{stored}");
            assert_eq!(editor_html_to_markdown(&stored), markdown, "{stored}");
        }
    }

    #[test]
    fn underline_subscript_and_superscript_stay_html() {
        assert_converts(
            "<u>under</u>, H<sub>2</sub>O and x<sup>2</sup>",
            "<p><u>under</u>, H<sub>2</sub>O and x<sup>2</sup></p>",
        );
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
    fn figures_show_known_sizes() {
        let uploads = Uploads {
            sizes: HashMap::from([
                ("abc.png".to_string(), "879 KB".to_string()),
                ("def.pdf".to_string(), "1.2 MB".to_string()),
            ]),
            ..Uploads::default()
        };
        let html =
            markdown_to_editor_html("![a.png](/raw/abc.png)\n\n[b.pdf](/raw/def.pdf)", &uploads);
        assert_eq!(
            html,
            "<figure class=\"inline-asset\" data-kind=\"image\"><img src=\"/raw/abc.png\" alt=\"a.png\">\
             <figcaption>a.png · 879 KB · <a href=\"/dl/abc.png\">download</a></figcaption></figure>\
             <figure class=\"inline-asset\" data-kind=\"pdf\" data-src=\"/raw/def.pdf\">\
             <figcaption>b.pdf · 1.2 MB · <a href=\"/raw/def.pdf\">open</a> · \
             <a href=\"/dl/def.pdf\">download</a></figcaption></figure>"
        );
        assert_eq!(
            editor_html_to_markdown(&html),
            "![a.png](/raw/abc.png)\n\n[b.pdf](/raw/def.pdf)"
        );
    }

    #[test]
    fn links_into_this_server_become_relative() {
        let uploads = Uploads {
            origin: Some("http://bin.lan".to_string()),
            ..Uploads::default()
        };
        let html = markdown_to_editor_html(
            "![x](http://bin.lan/raw/abc.png)\n\n[pad](http://bin.lan/pad/abc) \
             [other](http://bin.lan.evil/raw/abc.png) [same](http://bin.lan//evil.com/x) `http://bin.lan/x`",
            &uploads,
        );
        assert_eq!(
            html,
            "<figure class=\"inline-asset\" data-kind=\"image\"><img src=\"/raw/abc.png\" alt=\"x\">\
             <figcaption>x · <a href=\"/dl/abc.png\">download</a></figcaption></figure>\
             <p><a href=\"/pad/abc\">pad</a> <a href=\"http://bin.lan.evil/raw/abc.png\">other</a> \
             <a href=\"http://bin.lan//evil.com/x\">same</a> <code>http://bin.lan/x</code></p>"
        );
    }

    #[test]
    fn links_into_this_server_match_loosely() {
        let uploads = Uploads {
            origin: Some("http://bin.lan".to_string()),
            sizes: HashMap::from([("def.png".to_string(), "1 KB".to_string())]),
        };
        let html = markdown_to_editor_html(
            "[a](HTTP://Bin.LAN/pad/a) [b](http://bin.lan:80/pad/b) \
             [c](http://10.0.0.2:8088/raw/def.png) [d](https://example.com/raw/zzz.png) \
             [e](http://bin.lan:8080/pad/e)",
            &uploads,
        );
        assert_eq!(
            html,
            "<p><a href=\"/pad/a\">a</a> <a href=\"/pad/b\">b</a> <a href=\"/raw/def.png\">c</a> \
             <a href=\"https://example.com/raw/zzz.png\">d</a> <a href=\"http://bin.lan:8080/pad/e\">e</a></p>"
        );
        // A known upload at another host is a figure.
        let html = markdown_to_editor_html("![x](https://LOCALHOST:9/raw/def.png)", &uploads);
        assert!(html.starts_with("<figure"), "{html}");
        assert!(html.contains("<img src=\"/raw/def.png\""), "{html}");
    }

    #[test]
    fn code_text_becomes_editor_lines() {
        assert_eq!(
            text_to_editor_html("a\n\n  b<"),
            "<div>a</div><div><br></div><div>  b&lt;</div>"
        );
        assert_eq!(text_to_editor_html(""), "");
    }

    #[test]
    fn figures_between_text() {
        assert_round_trip(
            "Intro\n\n![a.png](/raw/a.png)\n\n![b.png](/raw/b.png)\n\n[c.pdf](/raw/c.pdf)\n\nOutro",
        );
        // Assets on consecutive lines are one paragraph in Markdown, separate figures here.
        let html = to_html("![a.png](/raw/a.png)\n![b.png](/raw/b.png)");
        assert_eq!(html.matches("<figure").count(), 2, "{html}");
        assert!(!html.contains("<p>"), "{html}");
    }

    #[test]
    fn figure_without_name_uses_the_file() {
        let html = to_html("![](/raw/abc.png)");
        assert!(html.contains("alt=\"abc.png\""), "{html}");
        assert_eq!(editor_html_to_markdown(&html), "![abc.png](/raw/abc.png)");
    }

    #[test]
    fn figure_name_is_escaped() {
        assert_round_trip("![a \\[draft\\] \\*v2\\*.png](/raw/abc.png)");
        let html = to_html("![<b>x</b>\"y](/raw/abc.png)");
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
            to_html("![logo](https://example.com/logo.png)"),
            "<p><a href=\"https://example.com/logo.png\">logo</a></p>"
        );
        assert_eq!(to_html("![x](//example.com/x.png)"), "<p><a>x</a></p>");
    }

    #[test]
    fn output_is_sanitized() {
        let html = to_html(
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
        let html = to_html("# A\n\n- b\n- c\n\n| d |\n| - |\n| e |\n\n> f\n\n1. g\n\n   h\n2. i");
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
        let markdown = editor_html_to_markdown(&to_html("- [x] done\n- [ ] open"));
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
            let once = editor_html_to_markdown(&to_html(markdown));
            let twice = editor_html_to_markdown(&to_html(&once));
            assert_eq!(once, twice, "from: {markdown}");
            assert_eq!(to_html(&once), to_html(&twice));
        }
    }

    #[test]
    fn empty() {
        assert_eq!(to_html(""), "");
        assert_eq!(editor_html_to_markdown(""), "");
    }
}
