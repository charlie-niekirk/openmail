//! How a message is shown in dark mode, decided when it is cleaned.
//!
//! Mail is written for a white page. In dark mode each message gets one of
//! three treatments ([`DarkStyle`]), the way Apple Mail and Gmail go about
//! it:
//!
//! - **Its own** ([`DarkStyle::Native`]): a message with
//!   `@media (prefers-color-scheme: dark)` rules styles itself for a dark
//!   page, so it is shown on the dark page as it asks.
//! - **Adapted** ([`DarkStyle::Adapt`]): a message whose backgrounds are
//!   all light grays (or none), whose text isn't light, and that paints no
//!   background image: typical personal mail. Its colors are written as
//!   `var(--om-cN, color)` when it is cleaned, so the light page shows them
//!   as they are, and the dark page defines each `--om-cN` as the color
//!   adapted ([`color::adapt`](super::color::adapt)). Presentational
//!   attributes (`bgcolor`, `<font color>`) are matched by attribute
//!   selectors instead.
//! - **A light card** ([`DarkStyle::Card`]): anything else (a designed
//!   newsletter with colored panels, background images, colors it can't
//!   read) keeps its light colors on a light card, so nothing designed for
//!   white turns unreadable.

use std::collections::HashMap;

use super::color::{self, Parsed, Rgba, Role};

/// How a message is shown in dark mode (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DarkStyle {
    /// Light colors on a light card.
    #[default]
    Card,
    /// The message's own dark styles.
    Native,
    /// The message's colors adapted.
    Adapt {
        /// The color `--om-cN` stands for (as CSS) and the role it plays,
        /// for N from 0.
        colors: Vec<(Role, String)>,
        /// Colors in presentational attributes.
        attributes: Vec<AttributeColor>,
    },
}

/// A color a presentational attribute gives, matched by an attribute
/// selector in dark mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeColor {
    /// The element it is on, or `None` for any.
    pub element: Option<&'static str>,
    pub attribute: &'static str,
    /// The attribute's value as written (only `#`, letters and digits).
    pub value: String,
    pub role: Role,
    /// The color it names, as CSS.
    pub color: String,
}

/// What cleaning a message learns about its colors, and in the second
/// pass, the custom properties its colors become.
#[derive(Debug, Default)]
pub struct Probe {
    colors: Vec<(Role, String)>,
    index: HashMap<(Role, String), usize>,
    attributes: Vec<AttributeColor>,
    /// Whether colors are replaced by custom properties.
    rewrite: bool,
    /// A color it couldn't read, a background image, dark styles of its own.
    unknown: bool,
    background_image: bool,
    dark_scheme: bool,
    /// A color that rules adapting out.
    unadaptable: bool,
}

impl Probe {
    /// A probe for the second pass, replacing the colors the first found.
    pub fn rewriting(first: &Self) -> Self {
        Self {
            colors: first.colors.clone(),
            index: first.index.clone(),
            rewrite: true,
            ..Self::default()
        }
    }

    /// A color a declaration uses: its custom property's name when
    /// rewriting.
    pub fn color(&mut self, role: Role, color: Rgba) -> Option<String> {
        self.check(role, color);
        let key = (role, color.to_css());
        let next = self.colors.len();
        let index = *self.index.entry(key.clone()).or_insert_with(|| {
            self.colors.push(key);
            next
        });
        self.rewrite.then(|| format!("--om-c{index}"))
    }

    fn check(&mut self, role: Role, color: Rgba) {
        let adaptable = match role {
            Role::Background => color::adaptable_background(color),
            Role::Text => color::adaptable_text(color),
            Role::Border => true,
        };
        self.unadaptable |= !adaptable;
    }

    pub fn unknown_color(&mut self) {
        self.unknown = true;
    }

    pub fn background_image(&mut self) {
        self.background_image = true;
    }

    pub fn dark_scheme(&mut self) {
        self.dark_scheme = true;
    }

    /// A presentational attribute that may hold a color (`bgcolor`, or
    /// `color` on `<font>` and `<hr>`).
    pub fn attribute(&mut self, element: &str, attribute: &str, value: &str) {
        let (element, attribute, role) = match (element, attribute) {
            (_, "bgcolor") => (None, "bgcolor", Role::Background),
            ("font", "color") => (Some("font"), "color", Role::Text),
            ("hr", "color") => (Some("hr"), "color", Role::Border),
            _ => return,
        };
        let value = value.trim();
        match color::parse_value(value) {
            Parsed::Color(color)
                if value.chars().all(|c| c == '#' || c.is_ascii_alphanumeric()) =>
            {
                self.check(role, color);
                let attribute_color = AttributeColor {
                    element,
                    attribute,
                    value: value.to_owned(),
                    role,
                    color: color.to_css(),
                };
                if !self.attributes.contains(&attribute_color) {
                    self.attributes.push(attribute_color);
                }
            }
            Parsed::Neutral => {}
            // Browsers make a color of nearly any text here.
            _ => self.unknown = true,
        }
    }

    /// Whether the second pass is needed: the message is adapted and has
    /// colors to replace.
    pub fn needs_rewrite(&self) -> bool {
        matches!(self.style(), DarkStyle::Adapt { .. }) && !self.colors.is_empty()
    }

    /// The treatment the message gets.
    pub fn style(&self) -> DarkStyle {
        if self.dark_scheme {
            DarkStyle::Native
        } else if self.unknown || self.background_image || self.unadaptable {
            DarkStyle::Card
        } else {
            DarkStyle::Adapt {
                colors: self.colors.clone(),
                attributes: self.attributes.clone(),
            }
        }
    }
}
