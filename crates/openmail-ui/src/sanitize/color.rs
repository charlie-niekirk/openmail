//! Colors in message CSS, and how dark mode adapts them.
//!
//! [`parse`] reads a CSS color from its tokens (hex, `rgb()`/`rgba()`,
//! `hsl()`/`hsla()` and the named colors). [`adapt`] maps a color a message
//! wrote for a white page onto a dark one, by the role it plays:
//!
//! - **Backgrounds** (light and nearly gray, the only ones a message may
//!   have to be adapted) and **borders** of little color move from white
//!   towards the dark page and from black towards its text, keeping how far
//!   they stood from white.
//! - **Text** of little color moves from black towards the page's text
//!   color, keeping its rank: darker text stays more prominent.
//! - **Colorful** text and borders keep their hue and saturation and are
//!   lightened only as much as needed to read on the dark page (WCAG
//!   contrast 4.5 for text, 3 for borders), so brand colors stay
//!   recognizable.

use cssparser::{Parser, Token};

/// A color with channels from 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

/// What a color does where a message uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// Text: `color`, `<font color>`, decoration and caret colors.
    Text,
    /// A background: `background(-color)`, `bgcolor`.
    Background,
    /// A line or shadow: borders, outlines, rules, shadows, `<hr color>`.
    Border,
}

/// What a value token makes of a color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Parsed {
    Color(Rgba),
    /// `transparent`, `currentcolor` and the global keywords: nothing to
    /// adapt.
    Neutral,
    /// Not a color.
    None,
}

/// The color the value `css` names (a whole value, such as an attribute's).
/// Legacy attributes also write hex without `#`.
pub fn parse_value(css: &str) -> Parsed {
    let css = css.trim();
    if css.len() >= 3 && css.chars().all(|c| c.is_ascii_hexdigit()) {
        return hex(css).map_or(Parsed::None, Parsed::Color);
    }
    let mut parser = Parser::new(css);
    let Ok(token) = parser.next().cloned() else {
        return Parsed::None;
    };
    let parsed = match &token {
        Token::Function(name) => {
            let name = name.to_ascii_lowercase();
            parser
                .parse_nested_block(|args| {
                    Ok::<_, cssparser::ParseError<()>>(function(&name, args))
                })
                .unwrap_or(Parsed::None)
        }
        token => parse(token),
    };
    if parser.is_exhausted() {
        parsed
    } else {
        Parsed::None
    }
}

/// The color a single token names: a hex or a named color (functions are
/// read by [`function`]).
pub fn parse(token: &Token<'_>) -> Parsed {
    match token {
        Token::Hash(value) | Token::IDHash(value) => hex(value).map_or(Parsed::None, Parsed::Color),
        Token::Ident(name) => named(name),
        _ => Parsed::None,
    }
}

/// The color function `name` (lowercase) of `args`: `rgb()`, `rgba()`,
/// `hsl()` or `hsla()`, with commas or spaces and an optional alpha.
pub fn function(name: &str, args: &mut Parser<'_>) -> Parsed {
    if !matches!(name, "rgb" | "rgba" | "hsl" | "hsla") {
        return Parsed::None;
    }
    let mut values = Vec::new();
    while let Ok(token) = args.next() {
        match token {
            Token::Number { value, .. } => values.push((f64::from(*value), false)),
            Token::Percentage { unit_value, .. } => values.push((f64::from(*unit_value), true)),
            Token::Dimension { value, unit, .. } if unit.eq_ignore_ascii_case("deg") => {
                values.push((f64::from(*value), false));
            }
            Token::Comma | Token::Delim('/') => {}
            _ => return Parsed::None,
        }
    }
    let alpha = match values.get(3) {
        None => 1.0,
        Some(&(value, _)) => value.clamp(0.0, 1.0),
    };
    if !(3..=4).contains(&values.len()) {
        return Parsed::None;
    }
    let color = if name.starts_with("rgb") {
        let channel = |(value, percent): (f64, bool)| {
            if percent { value } else { value / 255.0 }.clamp(0.0, 1.0)
        };
        Rgba {
            r: channel(values[0]),
            g: channel(values[1]),
            b: channel(values[2]),
            a: alpha,
        }
    } else {
        // Saturation and lightness are percentages; bare numbers out of 100.
        let unit = |(value, percent): (f64, bool)| {
            if percent { value } else { value / 100.0 }.clamp(0.0, 1.0)
        };
        from_hsl(values[0].0 / 360.0, unit(values[1]), unit(values[2]), alpha)
    };
    Parsed::Color(color)
}

fn hex(value: &str) -> Option<Rgba> {
    let digits: Vec<u8> = value
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    let pair = |i: usize| f64::from(digits[i] * 16 + digits[i + 1]) / 255.0;
    let single = |i: usize| f64::from(digits[i] * 17) / 255.0;
    Some(match digits.len() {
        3 => Rgba {
            r: single(0),
            g: single(1),
            b: single(2),
            a: 1.0,
        },
        4 => Rgba {
            r: single(0),
            g: single(1),
            b: single(2),
            a: single(3),
        },
        6 => Rgba {
            r: pair(0),
            g: pair(2),
            b: pair(4),
            a: 1.0,
        },
        8 => Rgba {
            r: pair(0),
            g: pair(2),
            b: pair(4),
            a: pair(6),
        },
        _ => return None,
    })
}

fn named(name: &str) -> Parsed {
    let name = name.to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "transparent" | "currentcolor" | "inherit" | "initial" | "unset" | "revert" | "none"
    ) {
        return Parsed::Neutral;
    }
    NAMED_COLORS
        .iter()
        .find(|(known, _)| *known == name)
        .map_or(Parsed::None, |&(_, rgb)| {
            let channel = |shift: u32| f64::from((rgb >> shift) & 0xff) / 255.0;
            Parsed::Color(Rgba {
                r: channel(16),
                g: channel(8),
                b: channel(0),
                a: 1.0,
            })
        })
}

impl Rgba {
    /// `#rrggbb`, or `rgba()` if translucent.
    pub fn to_css(self) -> String {
        let channel = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        if self.a >= 1.0 {
            format!(
                "#{:02x}{:02x}{:02x}",
                channel(self.r),
                channel(self.g),
                channel(self.b)
            )
        } else {
            format!(
                "rgba({},{},{},{})",
                channel(self.r),
                channel(self.g),
                channel(self.b),
                (self.a * 1000.0).round() / 1000.0
            )
        }
    }

    /// Hue (0 to 1), saturation and lightness, as in CSS `hsl()`.
    fn hsl(self) -> (f64, f64, f64) {
        let max = self.r.max(self.g).max(self.b);
        let min = self.r.min(self.g).min(self.b);
        let l = (max + min) / 2.0;
        let d = max - min;
        if d < 1e-9 {
            return (0.0, 0.0, l);
        }
        let s = d / (1.0 - (2.0 * l - 1.0).abs());
        let h = if (max - self.r).abs() < 1e-9 {
            ((self.g - self.b) / d).rem_euclid(6.0)
        } else if (max - self.g).abs() < 1e-9 {
            (self.b - self.r) / d + 2.0
        } else {
            (self.r - self.g) / d + 4.0
        };
        (h / 6.0, s, l)
    }

    /// How colorful it is: the spread of its channels.
    fn chroma(self) -> f64 {
        self.r.max(self.g).max(self.b) - self.r.min(self.g).min(self.b)
    }

    /// HSL lightness.
    pub fn lightness(self) -> f64 {
        self.hsl().2
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f64 {
        let linear = |c: f64| {
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    fn mix(self, other: Self, t: f64) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
            a: self.a,
        }
    }
}

fn from_hsl(h: f64, s: f64, l: f64, a: f64) -> Rgba {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h6 = h.rem_euclid(1.0) * 6.0;
    let x = c * (1.0 - (h6.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match h6 as u8 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    Rgba {
        r: r + m,
        g: g + m,
        b: b + m,
        a,
    }
}

/// WCAG contrast ratio of two opaque colors.
pub fn contrast(a: Rgba, b: Rgba) -> f64 {
    let (la, lb) = (a.luminance(), b.luminance());
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The most color a background, or text, may have and still count as
/// gray.
const GRAY_CHROMA: f64 = 0.12;

/// Whether a message's background can be adapted: light and nearly gray
/// (white, off-white, light gray panels). Colorful or dark backgrounds are
/// part of a design, which keeps its light card.
pub fn adaptable_background(color: Rgba) -> bool {
    color.a < 0.05 || (color.lightness() >= 0.85 && color.chroma() <= GRAY_CHROMA)
}

/// Whether text in `color` can be adapted: anything but light text, which
/// only reads on a background of the message's own design.
pub fn adaptable_text(color: Rgba) -> bool {
    color.a < 0.05 || color.luminance() <= 0.5
}

/// `color`, which a message uses as `role` on a white page, for a dark page
/// with background `page` and text `text`.
pub fn adapt(role: Role, color: Rgba, page: Rgba, text: Rgba) -> Rgba {
    let gray = color.chroma() <= GRAY_CHROMA;
    let lightness = color.lightness();
    match role {
        // White becomes the page; each step darker, a step lighter.
        Role::Background => page.mix(text, (1.0 - lightness) * 1.2),
        Role::Border if gray => page.mix(text, (1.0 - lightness) * 0.9),
        // Black becomes the page's text; grays keep their rank.
        Role::Text if gray => page.mix(text, 1.0 - 0.75 * lightness),
        Role::Text => readable(color, page, 4.5),
        Role::Border => readable(color, page, 3.0),
    }
    .with_alpha(color.a)
}

impl Rgba {
    fn with_alpha(self, a: f64) -> Self {
        Self { a, ..self }
    }
}

/// `color` lightened, keeping its hue and saturation, until it has
/// `ratio` contrast with `page` (or as light as it gets).
fn readable(color: Rgba, page: Rgba, ratio: f64) -> Rgba {
    // A little over, so rounding to 8-bit channels stays above it.
    let ratio = ratio + 0.05;
    let opaque = color.with_alpha(1.0);
    if contrast(opaque, page) >= ratio {
        return opaque;
    }
    let (h, s, l) = opaque.hsl();
    let (mut low, mut high) = (l, 0.95);
    if contrast(from_hsl(h, s, high, 1.0), page) < ratio {
        return from_hsl(h, s, high, 1.0);
    }
    for _ in 0..20 {
        let mid = (low + high) / 2.0;
        if contrast(from_hsl(h, s, mid, 1.0), page) >= ratio {
            high = mid;
        } else {
            low = mid;
        }
    }
    from_hsl(h, s, high, 1.0)
}

/// The CSS named colors, as `0xrrggbb`.
#[rustfmt::skip]
const NAMED_COLORS: [(&str, u32); 148] = [
    ("aliceblue", 0xf0f8ff), ("antiquewhite", 0xfaebd7), ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4), ("azure", 0xf0ffff), ("beige", 0xf5f5dc), ("bisque", 0xffe4c4),
    ("black", 0x000000), ("blanchedalmond", 0xffebcd), ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2), ("brown", 0xa52a2a), ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0), ("chartreuse", 0x7fff00), ("chocolate", 0xd2691e),
    ("coral", 0xff7f50), ("cornflowerblue", 0x6495ed), ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c), ("cyan", 0x00ffff), ("darkblue", 0x00008b), ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b), ("darkgray", 0xa9a9a9), ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9), ("darkkhaki", 0xbdb76b), ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f), ("darkorange", 0xff8c00), ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000), ("darksalmon", 0xe9967a), ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b), ("darkslategray", 0x2f4f4f), ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1), ("darkviolet", 0x9400d3), ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff), ("dimgray", 0x696969), ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff), ("firebrick", 0xb22222), ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22), ("fuchsia", 0xff00ff), ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff), ("gold", 0xffd700), ("goldenrod", 0xdaa520), ("gray", 0x808080),
    ("green", 0x008000), ("greenyellow", 0xadff2f), ("grey", 0x808080), ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4), ("indianred", 0xcd5c5c), ("indigo", 0x4b0082), ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c), ("lavender", 0xe6e6fa), ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00), ("lemonchiffon", 0xfffacd), ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080), ("lightcyan", 0xe0ffff), ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3), ("lightgreen", 0x90ee90), ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1), ("lightsalmon", 0xffa07a), ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa), ("lightslategray", 0x778899), ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de), ("lightyellow", 0xffffe0), ("lime", 0x00ff00),
    ("limegreen", 0x32cd32), ("linen", 0xfaf0e6), ("magenta", 0xff00ff), ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa), ("mediumblue", 0x0000cd), ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db), ("mediumseagreen", 0x3cb371), ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a), ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585), ("midnightblue", 0x191970), ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1), ("moccasin", 0xffe4b5), ("navajowhite", 0xffdead),
    ("navy", 0x000080), ("oldlace", 0xfdf5e6), ("olive", 0x808000), ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500), ("orangered", 0xff4500), ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa), ("palegreen", 0x98fb98), ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093), ("papayawhip", 0xffefd5), ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f), ("pink", 0xffc0cb), ("plum", 0xdda0dd), ("powderblue", 0xb0e0e6),
    ("purple", 0x800080), ("rebeccapurple", 0x663399), ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f), ("royalblue", 0x4169e1), ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072), ("sandybrown", 0xf4a460), ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee), ("sienna", 0xa0522d), ("silver", 0xc0c0c0), ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd), ("slategray", 0x708090), ("slategrey", 0x708090),
    ("snow", 0xfffafa), ("springgreen", 0x00ff7f), ("steelblue", 0x4682b4), ("tan", 0xd2b48c),
    ("teal", 0x008080), ("thistle", 0xd8bfd8), ("tomato", 0xff6347), ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee), ("wheat", 0xf5deb3), ("white", 0xffffff), ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00), ("yellowgreen", 0x9acd32),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn color(css: &str) -> Rgba {
        match parse_value(css) {
            Parsed::Color(color) => color,
            other => panic!("{css}: {other:?}"),
        }
    }

    const PAGE: &str = "#1e1f20";
    const TEXT: &str = "#e3e3e3";

    fn dark(role: Role, css: &str) -> String {
        adapt(role, color(css), color(PAGE), color(TEXT)).to_css()
    }

    #[test]
    fn colors_parse() {
        for (css, hex) in [
            ("#fff", "#ffffff"),
            ("#FFFFFF", "#ffffff"),
            ("ffffff", "#ffffff"),
            ("#1155cc", "#1155cc"),
            ("rgb(204,204,204)", "#cccccc"),
            ("rgb(0 0 0)", "#000000"),
            ("RGB(100%, 0%, 0%)", "#ff0000"),
            ("hsl(0, 100%, 50%)", "#ff0000"),
            ("hsl(120deg 100% 25%)", "#008000"),
            ("White", "#ffffff"),
            ("rebeccapurple", "#663399"),
        ] {
            assert_eq!(color(css).to_css(), hex, "{css}");
        }
        assert_eq!(color("rgba(0,0,0,.5)").to_css(), "rgba(0,0,0,0.5)");
        assert_eq!(color("#00000080").a, 128.0 / 255.0);
        for css in ["transparent", "currentColor", "inherit"] {
            assert_eq!(parse_value(css), Parsed::Neutral, "{css}");
        }
        for css in [
            "solid", "var(--x)", "#ggg", "rgb(1,2)", "1px", "red blue", "",
        ] {
            assert_eq!(parse_value(css), Parsed::None, "{css}");
        }
    }

    #[test]
    fn only_light_grays_are_adaptable_backgrounds() {
        for css in [
            "#ffffff",
            "#f6f8fc",
            "#eeeeee",
            "#e0e0e0",
            "transparent",
            "rgba(0,0,0,0)",
        ] {
            if let Parsed::Color(c) = parse_value(css) {
                assert!(adaptable_background(c), "{css}");
            }
        }
        for css in [
            "#0b3d91", "#e8590c", "#ffff00", "#fff3e0", "#333333", "#999999",
        ] {
            assert!(!adaptable_background(color(css)), "{css}");
        }
        assert!(adaptable_text(color("#000")) && adaptable_text(color("#1155cc")));
        assert!(!adaptable_text(color("#ffffff")) && !adaptable_text(color("#cccccc")));
    }

    #[test]
    fn white_pages_and_black_text_swap() {
        assert_eq!(dark(Role::Background, "#ffffff"), PAGE);
        assert_eq!(dark(Role::Text, "#000000"), TEXT);
        // Grays keep their order.
        let l = |role, css| color(&dark(role, css)).lightness();
        assert!(l(Role::Text, "#000") > l(Role::Text, "#333"));
        assert!(l(Role::Text, "#333") > l(Role::Text, "#666"));
        assert!(l(Role::Background, "#fff") < l(Role::Background, "#eee"));
        assert!(l(Role::Border, "#ddd") < l(Role::Border, "#999"));
        // Gray text stays readable.
        for css in ["#000", "#333", "#666", "#777"] {
            let ratio = contrast(color(&dark(Role::Text, css)), color(PAGE));
            assert!(ratio >= 4.5, "{css}: {ratio}");
        }
    }

    #[test]
    fn brand_colors_keep_their_hue() {
        for css in ["#1155cc", "#d93025", "#188038", "#0b57d0", "navy"] {
            let before = color(css);
            let after = color(&dark(Role::Text, css));
            assert!(contrast(after, color(PAGE)) >= 4.5, "{css}");
            let (h0, s0, _) = before.hsl();
            let (h1, s1, _) = after.hsl();
            assert!(
                (h0 - h1).abs() < 0.01 && (s0 - s1).abs() < 0.02,
                "{css} → {after:?}"
            );
        }
        // Already readable: unchanged.
        assert_eq!(dark(Role::Text, "#8ab4f8"), "#8ab4f8");
        // Translucency is kept.
        assert_eq!(
            dark(Role::Background, "rgba(255,255,255,0.5)"),
            "rgba(30,31,32,0.5)"
        );
    }
}
