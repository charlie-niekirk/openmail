//! The app's assets: `gpui-component`'s default icons, the Lucide icons the
//! app adds to them, and the app's own icons. The app registers [`Assets`]
//! when it creates the GPUI application.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

// Lucide icons beyond gpui-component's default set.
gpui_kit_assets::icon_assets!(LucideIcons, [Paperclip]);

/// Gmail's importance marker.
pub const IMPORTANCE_ICON: &str = "icons/openmail/importance.svg";

/// The app's own icons, by path.
const OWN_ICONS: &[(&str, &[u8])] = &[(
    IMPORTANCE_ICON,
    include_bytes!("../assets/icons/importance.svg"),
)];

/// Every icon the UI uses. Register it with
/// `gpui_platform::application().with_assets(Assets)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = OWN_ICONS.iter().find(|(own, _)| *own == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        if let Some(bytes) = LucideIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit_assets::Assets.list(path)?;
        paths.extend(LucideIcons.list(path)?);
        paths.extend(
            OWN_ICONS
                .iter()
                .map(|(own, _)| *own)
                .filter(|own| own.starts_with(path))
                .map(SharedString::new_static),
        );
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use gpui_component::{IconName, IconNamed as _};

    use super::*;

    fn loads(path: &str) -> bool {
        Assets
            .load(path)
            .is_ok_and(|bytes| bytes.is_some_and(|bytes| !bytes.is_empty()))
    }

    #[test]
    fn serves_the_icons_the_thread_list_uses() {
        assert!(loads(IMPORTANCE_ICON));
        assert!(loads(&gpui_kit_assets::IconName::Paperclip.path()));
        assert!(loads(&IconName::Star.path()));
        assert!(loads(&IconName::StarFill.path()));
        assert!(!loads("icons/no-such-icon.svg"));
    }

    #[test]
    fn lists_every_source() {
        let icons = Assets.list("icons/").unwrap();
        assert!(icons.iter().any(|path| path == IMPORTANCE_ICON));
        assert!(icons.iter().any(|path| path.contains("paperclip")));
        assert!(icons.iter().any(|path| path.contains("star-fill")));
    }
}
