//! Where the main window was, and whether it can go back there. Kept apart
//! from `window_settings` (desktop only) so the rules are tested in the
//! headless build, where the test suite runs.

use serde::{Deserialize, Serialize};

/// Below this the sidebar and a document no longer fit side by side.
pub(crate) const WINDOW_MIN_WIDTH: f64 = 960.0;
pub(crate) const WINDOW_MIN_HEIGHT: f64 = 600.0;
/// A restored window must show at least this much of its top edge on some
/// display, in points, so its title bar can be reached and dragged.
const RESTORE_VISIBLE_WIDTH: f64 = 160.0;
const RESTORE_VISIBLE_HEIGHT: f64 = 40.0;

/// The window's outer position and inner size, in logical points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WindowGeometry {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

/// A display's bounds in logical points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DisplayBounds {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl WindowGeometry {
    /// The geometry to restore, or `None` when it should not be: a value
    /// that is not a number, or a window whose top edge is on no connected
    /// display (a display was unplugged, or the arrangement changed). The
    /// size is raised to the minimum and cut to the display it lands on.
    pub(crate) fn restorable_on(self, displays: &[DisplayBounds]) -> Option<WindowGeometry> {
        let finite = [self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite());
        if !finite || self.width <= 0.0 || self.height <= 0.0 {
            return None;
        }
        let display = displays.iter().find(|display| {
            let left = self.x.max(display.x);
            let right = (self.x + self.width).min(display.x + display.width);
            let top_is_inside = self.y >= display.y
                && self.y + RESTORE_VISIBLE_HEIGHT <= display.y + display.height;
            top_is_inside && right - left >= RESTORE_VISIBLE_WIDTH
        })?;
        Some(WindowGeometry {
            x: self.x,
            y: self.y,
            width: self
                .width
                .max(WINDOW_MIN_WIDTH)
                .min(display.width.max(WINDOW_MIN_WIDTH)),
            height: self
                .height
                .max(WINDOW_MIN_HEIGHT)
                .min(display.height.max(WINDOW_MIN_HEIGHT)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAPTOP: DisplayBounds = DisplayBounds {
        x: 0.0,
        y: 0.0,
        width: 1728.0,
        height: 1117.0,
    };
    /// A second display to the left of the laptop's.
    const LEFT: DisplayBounds = DisplayBounds {
        x: -2560.0,
        y: 0.0,
        width: 2560.0,
        height: 1440.0,
    };

    fn geometry(x: f64, y: f64, width: f64, height: f64) -> WindowGeometry {
        WindowGeometry { x, y, width, height }
    }

    #[test]
    fn a_window_on_a_connected_display_comes_back_where_it_was() {
        let saved = geometry(200.0, 120.0, 1300.0, 800.0);
        assert_eq!(saved.restorable_on(&[LAPTOP]), Some(saved));
        let on_left = geometry(-2000.0, 300.0, 1400.0, 900.0);
        assert_eq!(on_left.restorable_on(&[LAPTOP, LEFT]), Some(on_left));
    }

    #[test]
    fn a_window_on_an_unplugged_display_is_not_restored() {
        let on_left = geometry(-2000.0, 300.0, 1400.0, 900.0);
        assert_eq!(on_left.restorable_on(&[LAPTOP]), None);
        assert_eq!(on_left.restorable_on(&[]), None);
    }

    #[test]
    fn a_window_whose_title_bar_is_out_of_reach_is_not_restored() {
        // Above the display, below it, and with only a sliver on screen.
        assert_eq!(geometry(100.0, -50.0, 1200.0, 700.0).restorable_on(&[LAPTOP]), None);
        assert_eq!(geometry(100.0, 1100.0, 1200.0, 700.0).restorable_on(&[LAPTOP]), None);
        assert_eq!(geometry(1700.0, 100.0, 1200.0, 700.0).restorable_on(&[LAPTOP]), None);
    }

    #[test]
    fn a_restored_size_stays_between_the_minimum_and_the_display() {
        let small = geometry(100.0, 100.0, 400.0, 300.0)
            .restorable_on(&[LAPTOP])
            .unwrap();
        assert_eq!((small.width, small.height), (WINDOW_MIN_WIDTH, WINDOW_MIN_HEIGHT));
        let huge = geometry(0.0, 0.0, 5000.0, 4000.0)
            .restorable_on(&[LAPTOP])
            .unwrap();
        assert_eq!((huge.width, huge.height), (LAPTOP.width, LAPTOP.height));
    }

    #[test]
    fn values_that_are_not_numbers_are_not_restored() {
        assert_eq!(geometry(f64::NAN, 0.0, 1200.0, 700.0).restorable_on(&[LAPTOP]), None);
        assert_eq!(geometry(0.0, 0.0, f64::INFINITY, 700.0).restorable_on(&[LAPTOP]), None);
        assert_eq!(geometry(0.0, 0.0, 0.0, 700.0).restorable_on(&[LAPTOP]), None);
    }

    #[test]
    fn geometry_is_stored_in_camel_case() {
        let saved = geometry(1.0, 2.0, 1200.0, 700.0);
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(json, r#"{"x":1.0,"y":2.0,"width":1200.0,"height":700.0}"#);
        assert_eq!(serde_json::from_str::<WindowGeometry>(&json).unwrap(), saved);
    }
}
