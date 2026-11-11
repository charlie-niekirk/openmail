//! The widths of the sidebar and the thread list, which the user sets by
//! dragging the dividers after them; the reading pane takes the rest.
//!
//! [`PaneSizes`] is the user's preference, kept within each pane's range and
//! saved to `layout.json` in the data directory (not the mail database, as
//! it belongs to the user rather than an account). [`PaneSizes::fit`] gives
//! the widths to lay out in a window of some width, shrinking the panes
//! towards their minimums so the reading pane keeps
//! [`CONVERSATION_MIN_WIDTH`]; the preference itself is untouched, so the
//! panes widen again with the window. Plain data with no GPUI types, so it is
//! tested without a window.

use std::io;
use std::path::{Path, PathBuf};

use openmail_store::DataDir;
use serde::{Deserialize, Serialize};

/// The file, in the data directory, that holds the pane sizes.
pub const LAYOUT_FILE: &str = "layout.json";

/// How wide a pane may be, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidthRange {
    pub min: f32,
    pub default: f32,
    pub max: f32,
}

impl WidthRange {
    /// `width` within the range; the default if it is not a number.
    pub fn clamp(&self, width: f32) -> f32 {
        if width.is_finite() {
            width.clamp(self.min, self.max)
        } else {
            self.default
        }
    }
}

/// The sidebar's widths.
pub const SIDEBAR_WIDTH: WidthRange = WidthRange {
    min: 160.,
    default: 220.,
    max: 400.,
};

/// The thread list's widths.
pub const THREAD_LIST_WIDTH: WidthRange = WidthRange {
    min: 280.,
    default: 360.,
    max: 640.,
};

/// The narrowest the reading pane gets while the other panes can shrink.
pub const CONVERSATION_MIN_WIDTH: f32 = 300.;

/// A divider the user drags, named by the pane it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divider {
    Sidebar,
    ThreadList,
}

/// The sidebar's and thread list's widths, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneSizes {
    pub sidebar: f32,
    pub thread_list: f32,
}

impl Default for PaneSizes {
    fn default() -> Self {
        Self {
            sidebar: SIDEBAR_WIDTH.default,
            thread_list: THREAD_LIST_WIDTH.default,
        }
    }
}

impl PaneSizes {
    /// Each width within its pane's range.
    pub fn clamped(self) -> Self {
        Self {
            sidebar: SIDEBAR_WIDTH.clamp(self.sidebar),
            thread_list: THREAD_LIST_WIDTH.clamp(self.thread_list),
        }
    }

    /// The widths to lay out in a mailbox `width` wide: the thread list
    /// shrinks first, then the sidebar, down to their minimums, so the
    /// reading pane keeps [`CONVERSATION_MIN_WIDTH`] where it can.
    pub fn fit(self, width: f32) -> Self {
        let Self {
            mut sidebar,
            mut thread_list,
        } = self.clamped();
        let mut over = sidebar + thread_list + CONVERSATION_MIN_WIDTH - width;
        if over > 0. {
            let give = over.min(thread_list - THREAD_LIST_WIDTH.min);
            thread_list -= give;
            over -= give;
        }
        if over > 0. {
            sidebar -= over.min(sidebar - SIDEBAR_WIDTH.min);
        }
        Self {
            sidebar,
            thread_list,
        }
    }

    /// Moves `divider` to `x` pixels from the mailbox's left edge, in a
    /// mailbox `width` wide, keeping the pane within its range and leaving the
    /// reading pane [`CONVERSATION_MIN_WIDTH`]. Returns whether a width
    /// changed.
    pub fn drag(&mut self, divider: Divider, x: f32, width: f32) -> bool {
        if !x.is_finite() || !width.is_finite() {
            return false;
        }
        let shown = self.fit(width);
        let (range, wanted, others) = match divider {
            Divider::Sidebar => (SIDEBAR_WIDTH, x, shown.thread_list),
            Divider::ThreadList => (THREAD_LIST_WIDTH, x - shown.sidebar, shown.sidebar),
        };
        let room = width - others - CONVERSATION_MIN_WIDTH;
        let new = wanted.clamp(range.min, range.max.min(room).max(range.min));
        let pane = match divider {
            Divider::Sidebar => &mut self.sidebar,
            Divider::ThreadList => &mut self.thread_list,
        };
        if *pane == new {
            return false;
        }
        *pane = new;
        true
    }

    /// The sizes saved at `path`; the defaults if there are none or they
    /// can't be read.
    pub fn load(path: &Path) -> Self {
        let sizes = match std::fs::read(path) {
            Ok(json) => serde_json::from_slice(&json).unwrap_or_else(|err| {
                tracing::warn!(path = %path.display(), "ignoring unreadable pane sizes: {err}");
                Self::default()
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                tracing::warn!(path = %path.display(), "could not read pane sizes: {err}");
                Self::default()
            }
        };
        sizes.clamped()
    }

    /// Saves the sizes at `path`, creating its directory if needed. Writes a
    /// temporary file and renames it, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let partial = path.with_extension("json.partial");
        std::fs::write(&partial, json)?;
        std::fs::rename(&partial, path)
    }
}

/// Where the pane sizes are saved: [`LAYOUT_FILE`] in the data directory
/// (`OPENMAIL_DATA_DIR`, or the platform's). `None` if there is none.
pub fn layout_path() -> Option<PathBuf> {
    match DataDir::from_env_or_platform() {
        Ok(dir) => Some(dir.root().join(LAYOUT_FILE)),
        Err(err) => {
            tracing::warn!("pane sizes won't be saved: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDE: f32 = 1600.;

    fn sizes(sidebar: f32, thread_list: f32) -> PaneSizes {
        PaneSizes {
            sidebar,
            thread_list,
        }
    }

    #[test]
    fn defaults_are_within_range() {
        let default = PaneSizes::default();
        assert_eq!(default.clamped(), default);
        for range in [SIDEBAR_WIDTH, THREAD_LIST_WIDTH] {
            assert!(range.min <= range.default && range.default <= range.max);
        }
    }

    #[test]
    fn clamps_to_each_panes_range() {
        assert_eq!(sizes(10., 10_000.).clamped(), sizes(160., 640.));
        assert_eq!(
            sizes(f32::NAN, f32::INFINITY).clamped(),
            PaneSizes::default()
        );
        assert_eq!(sizes(250., 400.).clamped(), sizes(250., 400.));
    }

    #[test]
    fn fit_keeps_wide_windows_as_preferred() {
        assert_eq!(sizes(250., 500.).fit(WIDE), sizes(250., 500.));
    }

    #[test]
    fn fit_shrinks_the_list_then_the_sidebar() {
        // 250 + 500 + 300 = 1050: 50 over, all from the list.
        assert_eq!(sizes(250., 500.).fit(1000.), sizes(250., 450.));
        // The list stops at its minimum; the sidebar gives the rest.
        assert_eq!(sizes(250., 500.).fit(800.), sizes(220., 280.));
        // Both at their minimums: the reading pane gets what is left.
        assert_eq!(sizes(250., 500.).fit(500.), sizes(160., 280.));
    }

    #[test]
    fn the_smallest_window_fits_every_pane() {
        let min_window = f32::from(crate::main_window::MIN_SIZE.width);
        assert!(SIDEBAR_WIDTH.min + THREAD_LIST_WIDTH.min + CONVERSATION_MIN_WIDTH <= min_window);
        let fitted = sizes(400., 640.).fit(min_window);
        assert!(min_window - fitted.sidebar - fitted.thread_list >= CONVERSATION_MIN_WIDTH);
    }

    #[test]
    fn dragging_the_sidebar_divider() {
        let mut panes = PaneSizes::default();
        assert!(panes.drag(Divider::Sidebar, 300., WIDE));
        assert_eq!(panes, sizes(300., 360.));
        // Past the ends of its range.
        assert!(panes.drag(Divider::Sidebar, 20., WIDE));
        assert_eq!(panes.sidebar, SIDEBAR_WIDTH.min);
        assert!(panes.drag(Divider::Sidebar, 900., WIDE));
        assert_eq!(panes.sidebar, SIDEBAR_WIDTH.max);
        // Already there.
        assert!(!panes.drag(Divider::Sidebar, 950., WIDE));
    }

    #[test]
    fn dragging_the_list_divider_is_relative_to_the_sidebar() {
        let mut panes = PaneSizes::default();
        assert!(panes.drag(Divider::ThreadList, 220. + 500., WIDE));
        assert_eq!(panes, sizes(220., 500.));
        assert!(!panes.drag(Divider::ThreadList, f32::NAN, WIDE));
    }

    #[test]
    fn dragging_leaves_the_reading_pane_its_minimum() {
        let mut panes = PaneSizes::default();
        // 1000 wide, sidebar 220: the list can reach 1000 - 220 - 300 = 480.
        assert!(panes.drag(Divider::ThreadList, 900., 1000.));
        assert_eq!(panes.thread_list, 480.);
        // The sidebar can reach 1000 - 480 - 300 = 220, where it already is.
        assert!(!panes.drag(Divider::Sidebar, 380., 1000.));
        // In a window too narrow for that, a pane stays at its minimum.
        assert!(panes.drag(Divider::ThreadList, 900., 700.));
        assert_eq!(panes.thread_list, THREAD_LIST_WIDTH.min);
    }

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(LAYOUT_FILE);
        assert_eq!(PaneSizes::load(&path), PaneSizes::default());

        let saved = sizes(250., 420.);
        saved.save(&path).unwrap();
        assert_eq!(PaneSizes::load(&path), saved);
        assert!(!path.with_extension("json.partial").exists());

        // Saving again replaces the file.
        sizes(300., 500.).save(&path).unwrap();
        assert_eq!(PaneSizes::load(&path), sizes(300., 500.));
    }

    #[test]
    fn loading_clamps_and_tolerates_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAYOUT_FILE);

        std::fs::write(&path, r#"{"sidebar": 5, "thread_list": 9000}"#).unwrap();
        assert_eq!(PaneSizes::load(&path), sizes(160., 640.));

        // A missing field takes its default; unknown ones are ignored.
        std::fs::write(&path, r#"{"sidebar": 300, "theme": "dark"}"#).unwrap();
        assert_eq!(PaneSizes::load(&path), sizes(300., 360.));

        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(PaneSizes::load(&path), PaneSizes::default());
    }
}
