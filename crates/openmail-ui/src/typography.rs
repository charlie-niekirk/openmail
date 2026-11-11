//! The type scale. Text uses the theme's font (the system UI font); views pick
//! a [`TextRole`] instead of raw sizes so the scale stays consistent.

use gpui::{FontWeight, Pixels, Styled, px};

/// What a piece of text is, which sets its size, line height and weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRole {
    /// Large headings, such as an empty state's title.
    Title,
    /// A conversation's subject, a pane's heading.
    Heading,
    /// The default: thread rows, labels, message text.
    Body,
    /// Supporting text: snippets, addresses, counts.
    Secondary,
    /// The smallest text: dates, label chips, the title bar's status.
    Caption,
}

impl TextRole {
    /// Every role, largest first.
    pub const ALL: [Self; 5] = [
        Self::Title,
        Self::Heading,
        Self::Body,
        Self::Secondary,
        Self::Caption,
    ];

    /// The font size.
    pub fn size(self) -> Pixels {
        px(match self {
            Self::Title => 20.,
            Self::Heading => 16.,
            Self::Body => 14.,
            Self::Secondary => 13.,
            Self::Caption => 12.,
        })
    }

    /// The line height.
    pub fn line_height(self) -> Pixels {
        px(match self {
            Self::Title => 28.,
            Self::Heading => 22.,
            Self::Body => 20.,
            Self::Secondary => 18.,
            Self::Caption => 16.,
        })
    }

    /// The font weight.
    pub fn weight(self) -> FontWeight {
        match self {
            Self::Title | Self::Heading => FontWeight::SEMIBOLD,
            Self::Body | Self::Secondary | Self::Caption => FontWeight::NORMAL,
        }
    }
}

/// Sets an element's text to a [`TextRole`].
pub trait TypographyExt: Styled + Sized {
    fn text_role(self, role: TextRole) -> Self {
        self.text_size(role.size())
            .line_height(role.line_height())
            .font_weight(role.weight())
    }
}

impl<E: Styled> TypographyExt for E {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scale_shrinks_from_title_to_caption() {
        for pair in TextRole::ALL.windows(2) {
            let (larger, smaller) = (pair[0], pair[1]);
            assert!(larger.size() > smaller.size(), "{larger:?} > {smaller:?}");
            assert!(larger.line_height() > smaller.line_height());
            assert!(larger.weight() >= smaller.weight());
        }
    }

    #[test]
    fn lines_have_room_for_their_text() {
        for role in TextRole::ALL {
            let ratio = f32::from(role.line_height()) / f32::from(role.size());
            assert!((1.25..=1.5).contains(&ratio), "{role:?}: {ratio}");
        }
    }
}
