//! The app shell: which overlays are up, in what order, and which one Esc dismisses
//! (M1.11, #64).

use self::sheet::{InspectorPlacement, SheetDetent, SheetGeometry};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SheetDragFrame {
    pub(crate) translate_y_px: f64,
    pub(crate) released_fraction: f64,
    pub(crate) velocity_fraction_per_second: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SheetDrag {
    pointer_id: i32,
    start_y_px: f64,
    last_y_px: f64,
    last_time_ms: f64,
    viewport_height_px: f64,
    start_fraction: f64,
    frame: SheetDragFrame,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SheetRenderMetrics {
    pub(crate) full_height_vh: f64,
    pub(crate) rest_offset_vh: f64,
}

impl SheetDrag {
    pub(crate) fn new(
        pointer_id: i32,
        start_y_px: f64,
        start_time_ms: f64,
        viewport_height_px: f64,
        start_fraction: f64,
    ) -> Option<Self> {
        let values = [
            start_y_px,
            start_time_ms,
            viewport_height_px,
            start_fraction,
        ];
        if values.iter().any(|value| !value.is_finite()) || viewport_height_px <= 0.0 {
            return None;
        }
        Some(Self {
            pointer_id,
            start_y_px,
            last_y_px: start_y_px,
            last_time_ms: start_time_ms,
            viewport_height_px,
            start_fraction,
            frame: SheetDragFrame {
                translate_y_px: 0.0,
                released_fraction: start_fraction,
                velocity_fraction_per_second: 0.0,
            },
        })
    }

    pub(crate) fn admit(active: &mut Option<Self>, candidate: Self) -> bool {
        if active.is_some() {
            return false;
        }
        *active = Some(candidate);
        true
    }

    pub(crate) fn take_matching(active: &mut Option<Self>, pointer_id: i32) -> Option<Self> {
        if active
            .as_ref()
            .is_some_and(|drag| drag.pointer_id == pointer_id)
        {
            active.take()
        } else {
            None
        }
    }

    pub(crate) fn cancel_matching(active: &mut Option<Self>, pointer_id: i32) -> bool {
        Self::take_matching(active, pointer_id).is_some()
    }

    pub(crate) fn sample(
        &mut self,
        pointer_id: i32,
        y_px: f64,
        time_ms: f64,
    ) -> Option<SheetDragFrame> {
        if pointer_id != self.pointer_id || !y_px.is_finite() || !time_ms.is_finite() {
            return None;
        }
        let translate_y_px = y_px - self.start_y_px;
        let released_fraction = self.start_fraction - translate_y_px / self.viewport_height_px;
        let elapsed_seconds = (time_ms - self.last_time_ms) / 1_000.0;
        let velocity_fraction_per_second = if elapsed_seconds > 0.0 {
            (self.last_y_px - y_px) / self.viewport_height_px / elapsed_seconds
        } else {
            0.0
        };
        self.last_y_px = y_px;
        self.last_time_ms = time_ms;
        self.frame = SheetDragFrame {
            translate_y_px,
            released_fraction,
            velocity_fraction_per_second,
        };
        Some(self.frame)
    }

    pub(crate) fn frame(&self) -> SheetDragFrame {
        self.frame
    }

    pub(crate) fn pointer_id(&self) -> i32 {
        self.pointer_id
    }
}

pub(crate) fn sheet_render_metrics(
    geometry: &SheetGeometry,
    placement: InspectorPlacement,
) -> Option<SheetRenderMetrics> {
    let detent = placement.detent()?;
    let full = geometry.fraction(SheetDetent::Full) * 100.0;
    let visible = geometry.fraction(detent) * 100.0;
    Some(SheetRenderMetrics {
        full_height_vh: full,
        rest_offset_vh: full - visible,
    })
}

pub mod core;
/// ADR 0032's tripwire: no service worker, ever — see the module doc.
mod pwa_guard;
/// Where the inspector sits per mode, and the bottom sheet's detent model (M1.12, #65).
/// Pure decision logic, host-tested; nothing renders it yet — see the module doc.
pub mod sheet;

#[cfg(test)]
mod wiring_tests {
    use super::core::ShellMode;
    use super::sheet::{InspectorPlacement, SheetDetent, SheetGeometry, SheetState};
    use super::*;

    #[test]
    fn follow_finger_motion_is_render_only_until_sheet_state_resolves_release() {
        let geometry = SheetGeometry::new(0.2, 0.5, 0.9, 0.6).unwrap();
        let mut state = SheetState::new(ShellMode::Portrait);
        assert_eq!(
            state.drag_released(&geometry, 0.5, 0.0),
            Some(SheetDetent::Half)
        );

        let mut drag = SheetDrag::new(7, 500.0, 1_000.0, 1_000.0, 0.5).unwrap();
        let frame = drag.sample(7, 200.0, 3_000.0).unwrap();

        assert_eq!(frame.translate_y_px, -300.0);
        assert_eq!(frame.released_fraction, 0.8);
        assert_eq!(frame.velocity_fraction_per_second, 0.15);
        assert_eq!(
            state.placement(),
            InspectorPlacement::BottomSheet(SheetDetent::Half)
        );

        assert_eq!(
            state.drag_released(
                &geometry,
                frame.released_fraction,
                frame.velocity_fraction_per_second,
            ),
            Some(SheetDetent::Full)
        );
    }

    #[test]
    fn upward_and_downward_samples_have_model_signed_velocity() {
        let mut up = SheetDrag::new(1, 600.0, 0.0, 1_000.0, 0.5).unwrap();
        let up_frame = up.sample(1, 500.0, 100.0).unwrap();
        assert_eq!(up_frame.translate_y_px, -100.0);
        assert_eq!(up_frame.released_fraction, 0.6);
        assert_eq!(up_frame.velocity_fraction_per_second, 1.0);

        let down_frame = up.sample(1, 550.0, 150.0).unwrap();
        assert_eq!(down_frame.translate_y_px, -50.0);
        assert_eq!(down_frame.released_fraction, 0.55);
        assert_eq!(down_frame.velocity_fraction_per_second, -1.0);
    }

    #[test]
    fn invalid_drag_inputs_are_rejected() {
        assert!(SheetDrag::new(1, 10.0, 0.0, 0.0, 0.5).is_none());
        assert!(SheetDrag::new(1, f64::NAN, 0.0, 800.0, 0.5).is_none());
    }

    #[test]
    fn foreign_or_non_finite_samples_do_not_mutate_the_active_drag() {
        let mut drag = SheetDrag::new(1, 600.0, 10.0, 1_000.0, 0.5).unwrap();
        let before = drag;
        assert!(drag.sample(2, 500.0, 20.0).is_none());
        assert_eq!(drag, before);
        assert!(drag.sample(1, f64::NAN, 20.0).is_none());
        assert_eq!(drag, before);
    }

    #[test]
    fn active_pointer_owns_the_drag_until_its_matching_take() {
        let mut active = None;
        let mut first = SheetDrag::new(7, 600.0, 0.0, 1_000.0, 0.5).unwrap();
        let first_frame = first.sample(7, 500.0, 100.0).unwrap();
        let second = SheetDrag::new(8, 400.0, 200.0, 1_000.0, 0.25).unwrap();

        assert!(SheetDrag::admit(&mut active, first));
        assert!(!SheetDrag::admit(&mut active, second));
        assert_eq!(active.as_ref().unwrap().pointer_id(), 7);
        assert_eq!(active.as_ref().unwrap().frame(), first_frame);

        assert_eq!(SheetDrag::take_matching(&mut active, 8), None);
        assert_eq!(active.as_ref().unwrap().pointer_id(), 7);
        assert_eq!(active.as_ref().unwrap().frame(), first_frame);

        let taken = SheetDrag::take_matching(&mut active, 7).unwrap();
        assert_eq!(taken.pointer_id(), 7);
        assert_eq!(taken.frame(), first_frame);
        assert!(active.is_none());

        assert!(SheetDrag::admit(&mut active, second));
        assert_eq!(active.as_ref().unwrap().pointer_id(), 8);
    }

    #[test]
    fn cancel_discards_only_the_matching_pointer() {
        let first = SheetDrag::new(7, 600.0, 0.0, 1_000.0, 0.5).unwrap();
        let mut active = Some(first);

        assert!(!SheetDrag::cancel_matching(&mut active, 8));
        assert_eq!(active.as_ref().unwrap().pointer_id(), 7);

        assert!(SheetDrag::cancel_matching(&mut active, 7));
        assert!(active.is_none());
    }

    #[test]
    fn non_increasing_sample_time_updates_position_but_cannot_invent_a_flick() {
        let mut drag = SheetDrag::new(1, 600.0, 10.0, 1_000.0, 0.5).unwrap();
        let frame = drag.sample(1, 500.0, 10.0).unwrap();
        assert_eq!(frame.translate_y_px, -100.0);
        assert_eq!(frame.released_fraction, 0.6);
        assert_eq!(frame.velocity_fraction_per_second, 0.0);
    }

    #[test]
    fn render_metrics_come_from_geometry_and_placement() {
        let geometry = SheetGeometry::new(0.2, 0.5, 0.9, 0.6).unwrap();
        assert_eq!(
            sheet_render_metrics(
                &geometry,
                InspectorPlacement::BottomSheet(SheetDetent::Summary)
            ),
            Some(SheetRenderMetrics {
                full_height_vh: 90.0,
                rest_offset_vh: 70.0
            })
        );
        assert_eq!(
            sheet_render_metrics(
                &geometry,
                InspectorPlacement::BottomSheet(SheetDetent::Half)
            ),
            Some(SheetRenderMetrics {
                full_height_vh: 90.0,
                rest_offset_vh: 40.0
            })
        );
        assert_eq!(
            sheet_render_metrics(
                &geometry,
                InspectorPlacement::BottomSheet(SheetDetent::Full)
            ),
            Some(SheetRenderMetrics {
                full_height_vh: 90.0,
                rest_offset_vh: 0.0
            })
        );
        assert_eq!(
            sheet_render_metrics(&geometry, InspectorPlacement::RightColumn),
            None
        );
    }
}

#[cfg(target_arch = "wasm32")]
pub mod signals;
