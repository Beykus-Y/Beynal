// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Centralized UI panel composition, focus, Z-order, and modal routing.
//!
//! Ensures system panels (X-Ray, Settings, Beyrix Studio) never accidentally
//! overlap each other, Escape always closes the topmost active panel, hidden
//! panels never intercept input, and hit testing strictly matches visual Z-order.

/// Top-level system panel / modal mode currently open.
///
/// System panels are mutually exclusive so X-Ray, Settings, and Beyrix Studio
/// never render on top of each other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActiveSystemPanel {
    #[default]
    None,
    XRay,
    Settings,
    Studio,
}

/// Visual owner of an interactive surface in the current frame's Z-order stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceOwner {
    Widget,
    XRayIndicator,
    XRayPanel,
    Studio,
    Settings,
}

/// Bounding rectangle of a visible interactive UI surface emitted during the
/// frame, stored in visual draw order (bottom to top).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InteractiveSurface {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub owner: SurfaceOwner,
}

impl InteractiveSurface {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        self.w > 0.0
            && self.h > 0.0
            && px >= self.x
            && px <= self.x + self.w
            && py >= self.y
            && py <= self.y + self.h
    }
}

/// Centralized manager for panel visibility, Z-order surfaces, and input gating.
#[derive(Clone, Debug, Default)]
pub struct UiCompositor {
    pub active_panel: ActiveSystemPanel,
    /// Ordered bottom-to-top to match exact visual draw order.
    pub surfaces: Vec<InteractiveSurface>,
}

impl UiCompositor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle a system panel. Opening a panel automatically closes any other
    /// open system panel so they never stack or overlap.
    pub fn toggle_panel(&mut self, panel: ActiveSystemPanel) {
        if self.active_panel == panel {
            self.active_panel = ActiveSystemPanel::None;
        } else {
            self.active_panel = panel;
        }
    }

    pub fn open_panel(&mut self, panel: ActiveSystemPanel) {
        self.active_panel = panel;
    }

    /// Close the active system panel (e.g. on Escape). Returns `true` if a
    /// panel was closed and the Escape key should be consumed.
    pub fn close_active(&mut self) -> bool {
        if self.active_panel != ActiveSystemPanel::None {
            self.active_panel = ActiveSystemPanel::None;
            true
        } else {
            false
        }
    }

    pub fn is_open(&self, panel: ActiveSystemPanel) -> bool {
        self.active_panel == panel
    }

    pub fn clear_surfaces(&mut self) {
        self.surfaces.clear();
    }

    pub fn register_surface(&mut self, x: f32, y: f32, w: f32, h: f32, owner: SurfaceOwner) {
        if w > 0.0 && h > 0.0 {
            self.surfaces.push(InteractiveSurface { x, y, w, h, owner });
        }
    }

    /// Return the topmost visible interactive surface owner at `(px, py)`,
    /// matching visual Z-order (topmost drawn surface wins).
    pub fn topmost_owner_at(&self, px: f32, py: f32) -> Option<SurfaceOwner> {
        self.surfaces
            .iter()
            .rev()
            .find(|s| s.contains(px, py))
            .map(|s| s.owner)
    }

    /// Whether a pointer press at `(px, py)` must be consumed by UI rather
    /// than falling through to terminal text selection or PTY mouse reporting.
    pub fn blocks_terminal_pointer(&self, px: f32, py: f32) -> bool {
        if self.active_panel == ActiveSystemPanel::Studio {
            return true;
        }
        self.topmost_owner_at(px, py).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_panels_are_mutually_exclusive_and_escape_closes_topmost() {
        let mut comp = UiCompositor::new();
        assert_eq!(comp.active_panel, ActiveSystemPanel::None);

        comp.toggle_panel(ActiveSystemPanel::XRay);
        assert!(comp.is_open(ActiveSystemPanel::XRay));
        assert!(!comp.is_open(ActiveSystemPanel::Studio));

        // Opening Studio closes X-Ray automatically so they never overlap.
        comp.toggle_panel(ActiveSystemPanel::Studio);
        assert!(comp.is_open(ActiveSystemPanel::Studio));
        assert!(!comp.is_open(ActiveSystemPanel::XRay));

        // Opening Settings closes Studio automatically.
        comp.toggle_panel(ActiveSystemPanel::Settings);
        assert!(comp.is_open(ActiveSystemPanel::Settings));
        assert!(!comp.is_open(ActiveSystemPanel::Studio));

        // Escape closes the active panel and returns true; second Escape returns false.
        assert!(comp.close_active());
        assert_eq!(comp.active_panel, ActiveSystemPanel::None);
        assert!(!comp.close_active());
    }

    #[test]
    fn z_order_hit_testing_matches_draw_order_and_blocks_terminal_selection() {
        let mut comp = UiCompositor::new();
        comp.register_surface(10.0, 10.0, 200.0, 100.0, SurfaceOwner::Widget);
        comp.register_surface(50.0, 20.0, 200.0, 120.0, SurfaceOwner::XRayPanel);

        // Overlapping area resolves to the topmost drawn surface (XRayPanel).
        assert_eq!(
            comp.topmost_owner_at(80.0, 50.0),
            Some(SurfaceOwner::XRayPanel)
        );
        // Non-overlapping widget area resolves to Widget.
        assert_eq!(comp.topmost_owner_at(20.0, 50.0), Some(SurfaceOwner::Widget));
        // Empty terminal area resolves to None.
        assert_eq!(comp.topmost_owner_at(500.0, 500.0), None);
        assert!(comp.blocks_terminal_pointer(80.0, 50.0));
        assert!(!comp.blocks_terminal_pointer(500.0, 500.0));
    }
}
