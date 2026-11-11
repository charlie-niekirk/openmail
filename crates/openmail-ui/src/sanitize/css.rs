//! Filtering and scoping a message's CSS, so it styles only its own message.
//!
//! Message CSS (a `style` attribute, or a `<style>` element) is tokenized with
//! `cssparser` and rebuilt from the tokens it keeps:
//!
//! - **Declarations** that could escape the message or load something are
//!   dropped: `position: fixed`/`sticky` (overlays), values with bad tokens,
//!   nested blocks or loading functions (`image-set()`, `src()`, …). Every
//!   `url()` goes through a callback that rewrites it (`cid:` to `data:`, a
//!   remote image the user chose to show) or replaces it with `none`.
//! - **Rules** keep only `@media` and `@supports` (with their rules scoped in
//!   turn); `@import`, `@font-face`, `@keyframes` and every other at-rule are
//!   dropped.
//! - **Selectors** are prefixed with the message's scope (its wrapper's ID
//!   selector), so they only match inside it. A leading `html`, `body` or
//!   `:root` becomes the wrapper itself, ID selectors take the message's ID
//!   prefix (as its `id` attributes do), and selectors that reach for the
//!   wrapper's siblings (a leading `+` or `~`) drop their rule.
//!
//! Working on tokens rather than text means CSS escapes (`\70 osition`) are
//! compared decoded and written back escaped.
//!
//! A [`Visitor`] decides what each `url()` becomes and hears what dark mode
//! needs to know (see [`color`](super::color)): the colors declarations use
//! and the role they play, which it may replace, background images, and
//! whether the message has styles of its own for a dark page.

use cssparser::{CowRcStr, ParseError, ParseErrorKind, Parser, ToCss as _, Token};

use super::color::{self, Parsed, Rgba, Role};

/// What the CSS filter asks and tells about a message's CSS.
pub trait Visitor {
    /// What a `url()` becomes: `Some` URL to keep, or `None` for `none`.
    fn url(&mut self, url: &str) -> Option<String>;

    /// A color a declaration uses as `role`. `Some(name)` replaces it with
    /// `var(name, color)`: `name` must be a custom property name.
    fn color(&mut self, _role: Role, _color: Rgba) -> Option<String> {
        None
    }

    /// Something in a color's place that isn't one it can read, such as
    /// `var()`.
    fn unknown_color(&mut self, _role: Role) {}

    /// A background image or gradient.
    fn background_image(&mut self) {}

    /// An `@media (prefers-color-scheme: dark)` rule: the message styles
    /// itself for a dark page.
    fn dark_scheme(&mut self) {}
}

/// A plain function decides only URLs.
impl<F: FnMut(&str) -> Option<String>> Visitor for F {
    fn url(&mut self, url: &str) -> Option<String> {
        self(url)
    }
}

/// How deeply blocks may nest before a stylesheet or declaration is dropped.
const MAX_DEPTH: usize = 32;

/// How deeply `@media` and `@supports` rules may nest.
const MAX_RULE_DEPTH: usize = 4;

/// Functions whose arguments load resources without `url()`, or run code in
/// old engines.
const BLOCKED_FUNCTIONS: [&str; 8] = [
    "image-set",
    "-webkit-image-set",
    "src",
    "element",
    "-moz-element",
    "paint",
    "expression",
    "image",
];

/// Properties dropped whatever their value.
const BLOCKED_PROPERTIES: [&str; 2] = ["behavior", "-moz-binding"];

/// The values `position` may take: those that keep an element in its
/// message's flow (the message's box clips `absolute` ones).
const ALLOWED_POSITIONS: [&str; 7] = [
    "static", "relative", "absolute", "inherit", "initial", "unset", "revert",
];

/// What a message's stylesheet is scoped to.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    /// The selector for the message's wrapper, such as `#b-123`.
    pub selector: &'a str,
    /// What the message's IDs are prefixed with, such as `b-123-`.
    pub id_prefix: &'a str,
}

/// A token, or a block with its opening token and contents.
#[derive(Debug, Clone)]
enum Node<'i> {
    Token(Token<'i>),
    /// A function, `(…)`, `[…]` or `{…}` block.
    Block(Token<'i>, Vec<Node<'i>>),
    /// CSS written as it is (a [`Visitor`]'s replacement color).
    Raw(String),
}

/// A `style` attribute's declarations, filtered. `urls` decides what each
/// `url()` becomes (see [`Visitor`]).
pub fn filter_declarations(css: &str, urls: &mut dyn Visitor) -> String {
    parse(css)
        .map(|nodes| declarations(&nodes, urls))
        .unwrap_or_default()
}

/// A `<style>` element's stylesheet, filtered and scoped to `scope`.
pub fn scope_stylesheet(css: &str, scope: Scope<'_>, urls: &mut dyn Visitor) -> String {
    let css = parse(css)
        .map(|nodes| rules(&nodes, scope, urls, 0))
        .unwrap_or_default();
    // It goes back inside a `<style>` element: `<` (as in `</style>`) only
    // ever appears in strings or as an invalid delimiter, so escaping it
    // changes nothing else.
    css.replace('<', "\\3c ")
}

fn parse(css: &str) -> Option<Vec<Node<'_>>> {
    let mut parser = Parser::new(css);
    tree(&mut parser, 0)
}

/// The tokens left in `parser` as a tree, or `None` if blocks nest too deep.
fn tree<'i>(parser: &mut Parser<'i>, depth: usize) -> Option<Vec<Node<'i>>> {
    let mut nodes = Vec::new();
    while let Ok(token) = parser.next_including_whitespace_and_comments() {
        let token = token.clone();
        match token {
            Token::Function(_)
            | Token::ParenthesisBlock
            | Token::SquareBracketBlock
            | Token::CurlyBracketBlock => {
                if depth >= MAX_DEPTH {
                    return None;
                }
                let children = parser
                    .parse_nested_block(|parser| {
                        tree(parser, depth + 1).ok_or(ParseError {
                            kind: ParseErrorKind::Custom(()),
                        })
                    })
                    .ok()?;
                nodes.push(Node::Block(token, children));
            }
            token => nodes.push(Node::Token(token)),
        }
    }
    Some(nodes)
}

fn write(nodes: &[Node<'_>], out: &mut String) {
    for node in nodes {
        match node {
            // A space keeps the tokens on either side apart.
            Node::Token(Token::Comment(_)) => out.push(' '),
            Node::Token(token) => {
                let _ = token.to_css(out);
            }
            Node::Raw(css) => out.push_str(css),
            Node::Block(open, children) => {
                let _ = open.to_css(out);
                write(children, out);
                out.push(match open {
                    Token::CurlyBracketBlock => '}',
                    Token::SquareBracketBlock => ']',
                    _ => ')',
                });
            }
        }
    }
}

fn to_string(nodes: &[Node<'_>]) -> String {
    let mut out = String::new();
    write(nodes, &mut out);
    out
}

fn is_blank(node: &Node<'_>) -> bool {
    matches!(
        node,
        Node::Token(Token::WhiteSpace(_) | Token::Comment(_) | Token::CDO | Token::CDC)
    )
}

/// `nodes` without leading and trailing whitespace and comments.
fn trim<'a, 'i>(nodes: &'a [Node<'i>]) -> &'a [Node<'i>] {
    let start = nodes
        .iter()
        .position(|n| !is_blank(n))
        .unwrap_or(nodes.len());
    let end = nodes
        .iter()
        .rposition(|n| !is_blank(n))
        .map_or(start, |i| i + 1);
    &nodes[start..end]
}

fn is_token(node: &Node<'_>, wanted: &Token<'_>) -> bool {
    matches!(node, Node::Token(token) if token == wanted)
}

// Declarations.

fn declarations(nodes: &[Node<'_>], urls: &mut dyn Visitor) -> String {
    nodes
        .split(|node| is_token(node, &Token::Semicolon))
        .filter_map(|declaration| self::declaration(declaration, urls))
        .collect::<Vec<_>>()
        .join(";")
}

/// One `name: value` declaration, filtered, or `None` to drop it.
fn declaration(nodes: &[Node<'_>], urls: &mut dyn Visitor) -> Option<String> {
    let (Node::Token(Token::Ident(name)), rest) = trim(nodes).split_first()? else {
        return None;
    };
    let (colon, value) = trim(rest).split_first()?;
    if !is_token(colon, &Token::Colon) {
        return None;
    }
    let value = trim(value);
    let property = name.to_ascii_lowercase();
    if value.is_empty()
        || BLOCKED_PROPERTIES.contains(&property.as_str())
        || !safe_value(value)
        || (property == "position" && !allowed_position(value))
    {
        return None;
    }
    if is_background(&property) && has_image(value) {
        urls.background_image();
    }
    let value = rewrite_urls(value, urls);
    let value = match color_role(&property) {
        Some(role) => rewrite_colors(&value, role, urls),
        None => value,
    };
    let mut out = String::new();
    let _ = Token::Ident(name.clone()).to_css(&mut out);
    out.push(':');
    write(&value, &mut out);
    Some(out)
}

/// Whether a value has no bad tokens, nested `{}` blocks, at-keywords or
/// blocked functions.
fn safe_value(nodes: &[Node<'_>]) -> bool {
    nodes.iter().all(|node| match node {
        Node::Token(token) => !matches!(
            token,
            Token::BadUrl(_)
                | Token::BadString(_)
                | Token::AtKeyword(_)
                | Token::CloseCurlyBracket
                | Token::CDO
                | Token::CDC
        ),
        Node::Block(Token::CurlyBracketBlock, _) => false,
        Node::Block(Token::Function(name), children) => {
            !BLOCKED_FUNCTIONS.contains(&name.to_ascii_lowercase().as_str()) && safe_value(children)
        }
        Node::Block(_, children) => safe_value(children),
        Node::Raw(_) => true,
    })
}

/// Whether a `position` value is a single allowed keyword (so `var()` can't
/// smuggle in `fixed`), optionally `!important`.
fn allowed_position(value: &[Node<'_>]) -> bool {
    let words: Vec<&Node<'_>> = value.iter().filter(|node| !is_blank(node)).collect();
    let keyword = match words.as_slice() {
        [Node::Token(Token::Ident(keyword))] => keyword,
        [
            Node::Token(Token::Ident(keyword)),
            Node::Token(Token::Delim('!')),
            Node::Token(Token::Ident(important)),
        ] if important.eq_ignore_ascii_case("important") => keyword,
        _ => return false,
    };
    ALLOWED_POSITIONS.contains(&keyword.to_ascii_lowercase().as_str())
}

/// The role colors play in `property`, if it is one dark mode adapts.
fn color_role(property: &str) -> Option<Role> {
    match property {
        "color"
        | "-webkit-text-fill-color"
        | "-webkit-text-stroke-color"
        | "text-decoration"
        | "text-decoration-color"
        | "text-emphasis-color"
        | "caret-color" => Some(Role::Text),
        "background" | "background-color" => Some(Role::Background),
        "outline" | "outline-color" | "box-shadow" | "text-shadow" | "column-rule"
        | "column-rule-color" => Some(Role::Border),
        border
            if border.starts_with("border")
                && !["radius", "width", "style", "collapse", "spacing", "image"]
                    .iter()
                    .any(|part| border.contains(part)) =>
        {
            Some(Role::Border)
        }
        _ => None,
    }
}

fn is_background(property: &str) -> bool {
    matches!(
        property,
        "background" | "background-image" | "-webkit-background-image"
    )
}

/// Whether a background value paints an image: a `url()` or a gradient.
fn has_image(nodes: &[Node<'_>]) -> bool {
    has_url(nodes)
        || nodes.iter().any(|node| match node {
            Node::Block(Token::Function(name), children) => {
                name.to_ascii_lowercase().ends_with("gradient") || has_image(children)
            }
            _ => false,
        })
}

/// `nodes`, a value whose colors play `role`, with the colors `visitor`
/// replaces replaced. Only the value's own colors count, not those inside
/// other functions (gradients count as images).
fn rewrite_colors<'i>(nodes: &[Node<'i>], role: Role, visitor: &mut dyn Visitor) -> Vec<Node<'i>> {
    nodes
        .iter()
        .map(|node| {
            let parsed = match node {
                Node::Token(token) => color::parse(token),
                Node::Block(Token::Function(name), _) => {
                    let name = name.to_ascii_lowercase();
                    if matches!(name.as_str(), "var" | "env" | "attr") {
                        visitor.unknown_color(role);
                    }
                    if matches!(name.as_str(), "rgb" | "rgba" | "hsl" | "hsla") {
                        color::parse_value(&to_string(std::slice::from_ref(node)))
                    } else {
                        Parsed::None
                    }
                }
                _ => Parsed::None,
            };
            match parsed {
                Parsed::Color(color) => visitor.color(role, color).map_or_else(
                    || node.clone(),
                    |css| {
                        Node::Raw(format!(
                            "var({css},{})",
                            to_string(std::slice::from_ref(node))
                        ))
                    },
                ),
                _ => node.clone(),
            }
        })
        .collect()
}

/// `nodes` with each `url()` replaced by what `urls` makes of it.
fn rewrite_urls<'i>(nodes: &[Node<'i>], urls: &mut dyn Visitor) -> Vec<Node<'i>> {
    let replace = |url: &str, urls: &mut dyn Visitor| match urls.url(url) {
        Some(url) => Node::Token(Token::UnquotedUrl(CowRcStr::from(url))),
        None => Node::Token(Token::Ident(CowRcStr::from("none"))),
    };
    nodes
        .iter()
        .map(|node| match node {
            Node::Token(Token::UnquotedUrl(url)) => replace(url, urls),
            Node::Block(Token::Function(name), children) if name.eq_ignore_ascii_case("url") => {
                match trim(children) {
                    [Node::Token(Token::QuotedString(url))] => replace(url, urls),
                    // `url()` with modifiers, or not a string: drop it.
                    _ => Node::Token(Token::Ident(CowRcStr::from("none"))),
                }
            }
            Node::Block(open, children) => Node::Block(open.clone(), rewrite_urls(children, urls)),
            node => node.clone(),
        })
        .collect()
}

/// Whether `nodes` hold a URL anywhere.
fn has_url(nodes: &[Node<'_>]) -> bool {
    nodes.iter().any(|node| match node {
        Node::Token(token) => matches!(token, Token::UnquotedUrl(_) | Token::BadUrl(_)),
        Node::Block(Token::Function(name), _) if name.eq_ignore_ascii_case("url") => true,
        Node::Block(_, children) => has_url(children),
        Node::Raw(_) => false,
    })
}

// Rules.

fn rules(nodes: &[Node<'_>], scope: Scope<'_>, urls: &mut dyn Visitor, depth: usize) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < nodes.len() {
        if is_blank(&nodes[i]) {
            i += 1;
            continue;
        }
        let is_block = |node: &Node<'_>| matches!(node, Node::Block(Token::CurlyBracketBlock, _));
        if let Node::Token(Token::AtKeyword(name)) = &nodes[i] {
            // An at-rule ends at a `;` or after its `{}` block.
            let end = nodes[i + 1..]
                .iter()
                .position(|node| is_token(node, &Token::Semicolon) || is_block(node))
                .map_or(nodes.len(), |n| i + 1 + n);
            let prelude = &nodes[i + 1..end];
            let keep = matches!(name.to_ascii_lowercase().as_str(), "media" | "supports")
                && depth < MAX_RULE_DEPTH
                && safe_value(prelude)
                && !has_url(prelude);
            if keep && let Some(Node::Block(Token::CurlyBracketBlock, body)) = nodes.get(end) {
                if prefers_dark(prelude) {
                    urls.dark_scheme();
                }
                let inner = rules(body, scope, urls, depth + 1);
                if !inner.is_empty() {
                    let _ = Token::AtKeyword(name.clone()).to_css(&mut out);
                    write(prelude, &mut out);
                    out.push('{');
                    out.push_str(&inner);
                    out.push('}');
                }
            }
            i = end + 1;
            continue;
        }

        // A style rule: a selector list, then a `{}` block of declarations.
        let Some(block) = nodes[i..].iter().position(is_block).map(|n| i + n) else {
            break;
        };
        if let Node::Block(_, body) = &nodes[block]
            && let Some(selectors) = scope_selectors(&nodes[i..block], scope)
        {
            let declarations = declarations(body, urls);
            if !declarations.is_empty() {
                out.push_str(&selectors);
                out.push('{');
                out.push_str(&declarations);
                out.push('}');
            }
        }
        i = block + 1;
    }
    out
}

/// Whether a media query asks for `prefers-color-scheme: dark`.
fn prefers_dark(prelude: &[Node<'_>]) -> bool {
    prelude.iter().any(|node| match node {
        Node::Block(Token::ParenthesisBlock, children) => {
            let words: Vec<String> = children
                .iter()
                .filter_map(|node| match node {
                    Node::Token(Token::Ident(word)) => Some(word.to_ascii_lowercase()),
                    _ => None,
                })
                .collect();
            (words.first().map(String::as_str) == Some("prefers-color-scheme")
                && words.get(1).map(String::as_str) == Some("dark"))
                || prefers_dark(children)
        }
        _ => false,
    })
}

/// A rule's selector list scoped to `scope`, or `None` to drop the rule.
fn scope_selectors(prelude: &[Node<'_>], scope: Scope<'_>) -> Option<String> {
    let mut scoped = Vec::new();
    for selector in prelude.split(|node| is_token(node, &Token::Comma)) {
        let selector = trim(selector);
        if selector.is_empty() || !safe_value(selector) || has_url(selector) {
            return None;
        }
        let selector = prefix_ids(selector, scope.id_prefix);
        let rest = to_string(strip_root(&selector));
        let rest = rest.trim();
        if rest.starts_with(['+', '~']) {
            return None;
        }
        scoped.push(if rest.is_empty() {
            scope.selector.to_owned()
        } else {
            format!("{} {rest}", scope.selector)
        });
    }
    Some(scoped.join(","))
}

/// `selector` with its ID selectors (`#name`) prefixed, in functional
/// pseudo-classes (`:not(#a)`) too.
fn prefix_ids<'i>(selector: &[Node<'i>], prefix: &str) -> Vec<Node<'i>> {
    selector
        .iter()
        .map(|node| match node {
            Node::Token(Token::IDHash(id)) => {
                Node::Token(Token::IDHash(CowRcStr::from(format!("{prefix}{id}"))))
            }
            Node::Block(open @ Token::Function(_), children) => {
                Node::Block(open.clone(), prefix_ids(children, prefix))
            }
            node => node.clone(),
        })
        .collect()
}

/// `selector` without the `html`, `body` and `:root` compounds it starts
/// with (and the `>` between them): they stand for the scope itself.
fn strip_root<'a, 'i>(mut selector: &'a [Node<'i>]) -> &'a [Node<'i>] {
    loop {
        let Some(after) = root_compound(selector) else {
            return selector;
        };
        let after = trim(after);
        // `html > body …`: keep going if another root compound follows.
        if let Some((first, next)) = after.split_first()
            && is_token(first, &Token::Delim('>'))
            && root_compound(trim(next)).is_some()
        {
            selector = trim(next);
        } else {
            selector = after;
        }
    }
}

/// What follows `selector`'s first compound if that compound is exactly
/// `html`, `body` or `:root`.
fn root_compound<'a, 'i>(selector: &'a [Node<'i>]) -> Option<&'a [Node<'i>]> {
    let rest = match selector {
        [Node::Token(Token::Ident(name)), rest @ ..]
            if name.eq_ignore_ascii_case("html") || name.eq_ignore_ascii_case("body") =>
        {
            rest
        }
        [
            Node::Token(Token::Colon),
            Node::Token(Token::Ident(name)),
            rest @ ..,
        ] if name.eq_ignore_ascii_case("root") => rest,
        _ => return None,
    };
    // The compound must end there: `body.dark` is not the root.
    match rest.first() {
        None => Some(rest),
        Some(node) if is_blank(node) => Some(rest),
        Some(Node::Token(Token::Delim('>' | '+' | '~'))) => Some(rest),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: Scope<'static> = Scope {
        selector: "#b-m1",
        id_prefix: "b-m1-",
    };

    fn block_all(_: &str) -> Option<String> {
        None
    }

    fn inline(css: &str) -> String {
        filter_declarations(css, &mut block_all)
    }

    fn sheet(css: &str) -> String {
        scope_stylesheet(css, SCOPE, &mut block_all)
    }

    #[test]
    fn declarations_keep_presentation() {
        assert_eq!(
            inline("color: #333; font: 14px/1.4 Arial, sans-serif ; margin:0 auto !important"),
            "color:#333;font:14px/1.4 Arial, sans-serif;margin:0 auto !important"
        );
        assert_eq!(inline("  "), "");
        assert_eq!(inline("color red; :x; width:"), "");
    }

    #[test]
    fn overlays_are_dropped() {
        for css in [
            "position: fixed; top: 0",
            "position:sticky;top:0",
            "position: -webkit-sticky; top: 0",
            "position: FIXED; top: 0",
            "\\70 osition: fi\\78 ed; top: 0",
            "--p: fixed; position: var(--p); top: 0",
        ] {
            let out = inline(css);
            assert!(!out.contains("position"), "{css} → {out}");
            assert!(out.ends_with("top:0"), "{css} → {out}");
        }
        assert_eq!(inline("position: relative"), "position:relative");
        assert_eq!(
            inline("position: absolute !important"),
            "position:absolute !important"
        );
    }

    #[test]
    fn urls_go_through_the_callback() {
        let mut seen = Vec::new();
        let out = filter_declarations(
            "background: #fff url(https://t.example/a.png) no-repeat; \
             background-image: url( 'cid:logo@x' ); list-style: u\\72 l(\"x.png\")",
            &mut |url: &str| {
                seen.push(url.to_owned());
                url.starts_with("cid:")
                    .then(|| "data:image/png;base64,AA==".to_owned())
            },
        );
        assert_eq!(seen, ["https://t.example/a.png", "cid:logo@x", "x.png"]);
        assert_eq!(
            out,
            "background:#fff none no-repeat;\
             background-image:url(data:image/png;base64,AA==);list-style:none"
        );
    }

    #[test]
    fn loading_functions_and_bad_tokens_are_dropped() {
        for css in [
            "background-image: image-set('https://t.example/a.png' 1x)",
            "background-image: -webkit-image-set(url(a.png) 1x)",
            "width: expression(alert(1))",
            "behavior: url(x.htc)",
            "-moz-binding: url(x.xml#x)",
            "background: url(https://t.example/a b.png)",
            "content: 'unterminated\n",
            "color: red { }",
            "background: url('https://t.example/a.png' format('png'))",
        ] {
            let out = inline(css);
            assert!(
                !out.contains("t.example") && !out.contains("x."),
                "{css} → {out}"
            );
        }
        assert_eq!(
            inline("background: url('https://t.example/a.png' x)"),
            "background:none"
        );
    }

    #[test]
    fn selectors_are_scoped_to_the_message() {
        assert_eq!(
            sheet("p, .x a:hover , table > td { color: red }"),
            "#b-m1 p,#b-m1 .x a:hover,#b-m1 table > td{color:red}"
        );
        assert_eq!(sheet("* { margin: 0 }"), "#b-m1 *{margin:0}");
    }

    #[test]
    fn root_selectors_become_the_scope() {
        assert_eq!(
            sheet("body { background: #eee } html, :root { --x: 1px }"),
            "#b-m1{background:#eee}#b-m1,#b-m1{--x:1px}"
        );
        assert_eq!(
            sheet("html body .x, body > table, HTML > BODY > div {color:red}"),
            "#b-m1 .x,#b-m1 > table,#b-m1 > div{color:red}"
        );
        // Not the root, so it matches nothing inside the message.
        assert_eq!(
            sheet("body.dark p {color:red}"),
            "#b-m1 body.dark p{color:red}"
        );
    }

    #[test]
    fn ids_take_the_message_prefix() {
        assert_eq!(
            sheet("#top, a:not(#x) { color: red }"),
            "#b-m1 #b-m1-top,#b-m1 a:not(#b-m1-x){color:red}"
        );
    }

    #[test]
    fn rules_reaching_outside_are_dropped() {
        assert_eq!(sheet("body ~ div, p { color: red }"), "");
        assert_eq!(sheet("+ p { color: red }"), "");
        assert_eq!(
            sheet("{ color: red } p { color: blue }"),
            "#b-m1 p{color:blue}"
        );
    }

    #[test]
    fn hostile_stylesheets_are_neutralized() {
        let css = r#"
            @import url("https://t.example/track.css");
            @import 'https://t.example/other.css';
            @charset "utf-8";
            @font-face { font-family: X; src: url(https://t.example/x.woff) }
            @keyframes spin { from { transform: rotate(0) } }
            @namespace svg url(http://www.w3.org/2000/svg);
            html, body { position: fixed; inset: 0; z-index: 99999; background: url(https://t.example/px.gif) }
            .overlay { position: fixed !important; top: 0; left: 0; width: 100vw; height: 100vh }
            @media screen and (max-width: 600px) { .col { width: 100% !important } }
            @media url(x) { p { color: red } }
            @supports (display: grid) { @media print { p { display: none } } }
            p { color: red; } </style><script>alert(1)</script>
        "#;
        let out = sheet(css);
        assert!(!out.contains("t.example"), "{out}");
        assert!(!out.contains("fixed"), "{out}");
        assert!(
            !out.contains("@import") && !out.contains("@font-face"),
            "{out}"
        );
        assert!(
            !out.contains("@keyframes") && !out.contains("@namespace"),
            "{out}"
        );
        assert!(!out.contains("</"), "{out}");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn deep_nesting_is_dropped() {
        let deep = format!(
            "p {{ width: {}1px{} }}",
            "calc(".repeat(100),
            ")".repeat(100)
        );
        assert_eq!(sheet(&deep), "");
        let nested = format!(
            "{}p {{ color: red }}{}",
            "@media screen {".repeat(10),
            "}".repeat(10)
        );
        assert_eq!(sheet(&nested), "");
    }
}
