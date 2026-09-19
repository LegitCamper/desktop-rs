/// Edge a surface slides in from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slide {
    Top,
    Right,
    Bottom,
    Left,
}

/// Slide-in, hold, slide-out timing for one surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Animation {
    pub slide: Slide,
    /// Travel in pixels, measured from the resting position outwards.
    pub distance: i32,
    /// Length of both the slide in and the slide out.
    pub duration_ms: u32,
    /// Time fully visible before sliding out. `None` never slides out.
    pub hold_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Moving; the surface needs another frame after this one.
    Moving,
    /// Resting at the target position with no pending frame.
    Resting,
    /// Slide out finished; the surface should close.
    Done,
}

impl Animation {
    /// Offset from the resting position at `elapsed_ms` after mapping.
    ///
    /// Positive values push the surface back towards `slide`'s edge.
    pub fn offset(&self, elapsed_ms: u64) -> (Phase, i32) {
        let duration = u64::from(self.duration_ms);
        let travel =
            |progress: f32| (self.distance as f32 * (1.0 - ease_out(progress))).round() as i32;

        if elapsed_ms < duration {
            let progress = elapsed_ms as f32 / duration.max(1) as f32;
            return (Phase::Moving, travel(progress));
        }

        let Some(hold_ms) = self.hold_ms else {
            return (Phase::Resting, 0);
        };
        let out_start = duration.saturating_add(u64::from(hold_ms));
        if elapsed_ms < out_start {
            return (Phase::Resting, 0);
        }

        let out_elapsed = elapsed_ms - out_start;
        if out_elapsed >= duration {
            return (Phase::Done, self.distance);
        }
        let progress = 1.0 - out_elapsed as f32 / duration.max(1) as f32;
        (Phase::Moving, travel(progress))
    }
}

/// Cubic ease-out: fast start, soft landing.
fn ease_out(progress: f32) -> f32 {
    let clamped = progress.clamp(0.0, 1.0);
    1.0 - (1.0 - clamped).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLIDE: Animation = Animation {
        slide: Slide::Right,
        distance: 400,
        duration_ms: 200,
        hold_ms: Some(1000),
    };

    #[test]
    fn offset_should_start_fully_off_screen() {
        assert_eq!(SLIDE.offset(0), (Phase::Moving, 400));
    }

    #[test]
    fn offset_should_rest_at_zero_after_sliding_in() {
        assert_eq!(SLIDE.offset(200), (Phase::Resting, 0));
        assert_eq!(SLIDE.offset(900), (Phase::Resting, 0));
    }

    #[test]
    fn offset_should_return_off_screen_after_hold() {
        let (phase, offset) = SLIDE.offset(1300);

        assert_eq!(phase, Phase::Moving);
        assert!(offset > 0 && offset < 400, "unexpected offset: {offset}");
    }

    #[test]
    fn offset_should_finish_once_the_slide_out_completes() {
        assert_eq!(SLIDE.offset(1400), (Phase::Done, 400));
    }

    #[test]
    fn offset_should_rest_forever_without_a_hold() {
        let sticky = Animation {
            hold_ms: None,
            ..SLIDE
        };

        assert_eq!(sticky.offset(10_000), (Phase::Resting, 0));
    }

    #[test]
    fn ease_out_should_cover_the_unit_interval() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert!(ease_out(0.5) > 0.5, "ease-out must lead a linear ramp");
    }
}
