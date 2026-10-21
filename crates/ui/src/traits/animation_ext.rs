use std::time::Duration;

use gpui::{Animation, AnimationElement, AnimationExt, Transformation, percentage};

use crate::{prelude::*, traits::transformable::Transformable};

/// Rate for ambient, looping animations (loading spinners, pulses), which step
/// at this rate instead of redrawing the window on every frame. A remote desktop
/// pays for every frame; at these sizes 15 steps a second still reads as smooth.
pub const AMBIENT_ANIMATION_STEPS_PER_SECOND: u32 = 15;

/// An extension trait for adding common animations to animatable components.
pub trait CommonAnimationExt: AnimationExt {
    /// Render this component as rotating over the given duration.
    ///
    /// NOTE: This method uses the location of the caller to generate an ID for this state.
    ///       If this is not sufficient to identify your state (e.g. you're rendering a list item),
    ///       you can provide a custom ElementID using the `use_keyed_rotate_animation` method.
    #[track_caller]
    fn with_rotate_animation(self, duration: u64) -> AnimationElement<Self>
    where
        Self: Transformable + Sized,
    {
        self.with_keyed_rotate_animation(
            ElementId::CodeLocation(*std::panic::Location::caller()),
            duration,
        )
    }

    /// Render this component as rotating with the given element ID over the given duration.
    fn with_keyed_rotate_animation(
        self,
        id: impl Into<ElementId>,
        duration: u64,
    ) -> AnimationElement<Self>
    where
        Self: Transformable + Sized,
    {
        self.with_animation(
            id,
            Animation::new(Duration::from_secs(duration))
                .repeat()
                .steps((duration as u32 * AMBIENT_ANIMATION_STEPS_PER_SECOND).max(1)),
            move |component, delta| {
                // Snap to the step so each redraw shows a new angle.
                let steps = (duration as u32 * AMBIENT_ANIMATION_STEPS_PER_SECOND).max(1) as f32;
                let delta = (delta * steps).floor() / steps;
                component.transform(Transformation::rotate(percentage(delta)))
            },
        )
    }
}

impl<T: AnimationExt> CommonAnimationExt for T {}
