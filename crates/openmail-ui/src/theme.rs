//! Openmail's light and dark themes, which follow the system appearance.
//!
//! The palettes live in `themes/openmail.json`, a `gpui-component` theme set:
//! colors it does not name fall back to `gpui-component`'s defaults. Text uses
//! the system UI font at [`TextRole::Body`](crate::TextRole::Body) size.

use std::rc::Rc;

use gpui::{App, Context, Subscription, Window};
use gpui_component::{Theme, ThemeConfig, ThemeMode, ThemeSet};

/// The theme set, embedded in the binary.
const THEMES_JSON: &str = include_str!("../themes/openmail.json");

/// Openmail's light and dark themes.
#[derive(Debug, Clone)]
pub struct Themes {
    pub light: ThemeConfig,
    pub dark: ThemeConfig,
}

impl Themes {
    /// Parses the embedded theme set.
    ///
    /// # Panics
    ///
    /// If it lacks a light or a dark theme, or is not valid JSON; the tests
    /// make sure it is neither.
    pub fn load() -> Self {
        let set: ThemeSet =
            serde_json::from_str(THEMES_JSON).expect("themes/openmail.json is a valid theme set");
        let find = |mode: ThemeMode| {
            set.themes
                .iter()
                .find(|theme| theme.mode == mode)
                .cloned()
                .unwrap_or_else(|| panic!("themes/openmail.json has no {mode:?} theme"))
        };
        Self {
            light: find(ThemeMode::Light),
            dark: find(ThemeMode::Dark),
        }
    }
}

/// Installs Openmail's themes and switches to the one matching the system
/// appearance. Call after `gpui_component::init`.
pub fn init(cx: &mut App) {
    let Themes { light, dark } = Themes::load();
    Theme::update(cx, |theme| {
        theme.light_theme = Rc::new(light);
        theme.dark_theme = Rc::new(dark);
    });
    // Loads the theme for the current appearance (even if the mode is
    // unchanged), so the new configs take effect.
    Theme::sync_system_appearance(None, cx);
}

/// Keeps the theme in step with `window`'s light or dark appearance for as
/// long as the returned subscription is held.
pub fn follow_system_appearance<V: 'static>(
    window: &mut Window,
    cx: &mut Context<'_, V>,
) -> Subscription {
    Theme::sync_system_appearance(Some(window), cx);
    cx.observe_window_appearance(window, |_, window, cx| {
        Theme::sync_system_appearance(Some(window), cx);
    })
}

#[cfg(test)]
mod tests {
    use gpui_component::try_parse_color;

    use super::*;
    use crate::TextRole;

    #[test]
    fn has_a_light_and_a_dark_theme() {
        let themes = Themes::load();
        assert_eq!(themes.light.mode, ThemeMode::Light);
        assert_eq!(themes.dark.mode, ThemeMode::Dark);
        assert_eq!(themes.light.name.as_ref(), "Openmail Light");
        assert_eq!(themes.dark.name.as_ref(), "Openmail Dark");
    }

    #[test]
    fn every_color_parses() {
        let set: serde_json::Value = serde_json::from_str(THEMES_JSON).unwrap();
        for theme in set["themes"].as_array().unwrap() {
            let colors = theme["colors"].as_object().unwrap();
            assert!(!colors.is_empty());
            for (key, value) in colors {
                let value = value.as_str().unwrap();
                assert!(
                    try_parse_color(value).is_ok(),
                    "{}: {key} = {value:?} is not a color",
                    theme["name"]
                );
            }
        }
    }

    #[test]
    fn both_themes_share_the_type_scale() {
        let themes = Themes::load();
        let body = f32::from(TextRole::Body.size());
        for theme in [&themes.light, &themes.dark] {
            assert_eq!(theme.font_size, Some(body), "{}", theme.name);
            // The system UI font.
            assert_eq!(theme.font_family, None, "{}", theme.name);
        }
        assert_eq!(themes.light.radius, themes.dark.radius);
    }

    #[test]
    fn both_themes_set_the_same_colors() {
        let set: serde_json::Value = serde_json::from_str(THEMES_JSON).unwrap();
        let keys = |i: usize| {
            let mut keys: Vec<_> = set["themes"][i]["colors"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            keys.sort();
            keys
        };
        assert_eq!(keys(0), keys(1));
    }
}
