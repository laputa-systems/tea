//! Bounded frame planning for the terminal projection.

use tea_tui::Rect;

/// Measured regions of a frame. The footer owns the composer, menu, and hint rows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameLayout {
    pub transcript: Rect,
    pub activity: Rect,
    pub footer: Rect,
    pub composer: Rect,
    pub menu: Rect,
    pub hint: Rect,
}

impl FrameLayout {
    /// Plan a transcript-first minimal frame. The composer follows the visible
    /// transcript until the terminal is full, then the transcript is the only
    /// region that scrolls behind the preserved input and affordance rows.
    pub fn plan_flow(
        width: u16,
        height: u16,
        transcript_rows: usize,
        activity_rows: usize,
        composer_rows: usize,
        menu_rows: usize,
        hint_rows: usize,
    ) -> Self {
        plan_flow(
            width,
            height,
            transcript_rows,
            activity_rows,
            composer_rows,
            menu_rows,
            hint_rows,
        )
    }
}

/// Plan the default minimal presentation as a top-to-bottom conversation flow.
///
/// A short transcript leaves unused rows below its context/status hint, matching
/// the captured fx minimal startup frame. Once content would exceed the terminal,
/// the transcript gets the remaining rows and the composer remains reachable.
pub fn plan_flow(
    width: u16,
    height: u16,
    transcript_rows: usize,
    activity_rows: usize,
    composer_rows: usize,
    menu_rows: usize,
    hint_rows: usize,
) -> FrameLayout {
    let composer_height = composer_rows.min(usize::from(height)) as u16;

    if menu_rows != 0 {
        // Completion is an inline surface: transcript, a single breathing row,
        // composer, then the measured menu. The menu includes its own divider
        // and navigation affordance, so no ordinary status hint is appended.
        let menu_height = menu_rows.min(usize::from(height.saturating_sub(composer_height))) as u16;
        let content_budget = height.saturating_sub(composer_height + menu_height);
        let transcript_height =
            transcript_rows.min(usize::from(content_budget.saturating_sub(1))) as u16;
        let gap = u16::from(transcript_height != 0 && transcript_height < content_budget);
        let composer_y = transcript_height + gap;
        let menu_y = composer_y.saturating_add(composer_height);
        return FrameLayout {
            transcript: Rect {
                x: 0,
                y: 0,
                width,
                height: transcript_height,
            },
            activity: Rect {
                x: 0,
                y: composer_y,
                width,
                height: 0,
            },
            footer: Rect {
                x: 0,
                y: composer_y,
                width,
                height: composer_height.saturating_add(menu_height),
            },
            composer: Rect {
                x: 0,
                y: composer_y,
                width,
                height: composer_height,
            },
            menu: Rect {
                x: 0,
                y: menu_y,
                width,
                height: menu_height,
            },
            hint: Rect {
                x: 0,
                y: menu_y.saturating_add(menu_height),
                width,
                height: 0,
            },
        };
    }

    let hint_height = hint_rows.min(usize::from(height.saturating_sub(composer_height))) as u16;
    let remaining = height.saturating_sub(composer_height + hint_height);
    // When there is room, retain one blank row after transcript/activity and
    // one before the status line. They disappear before input or status does.
    let after_composer_gap = u16::from(hint_height != 0 && remaining != 0);
    let after_gap_remaining = remaining.saturating_sub(after_composer_gap);
    let before_composer_gap = u16::from(transcript_rows != 0 && after_gap_remaining != 0);
    let content_budget = after_gap_remaining.saturating_sub(before_composer_gap);
    let activity_height = activity_rows.min(usize::from(content_budget)) as u16;
    let transcript_height =
        transcript_rows.min(usize::from(content_budget.saturating_sub(activity_height))) as u16;
    let activity_y = transcript_height.saturating_add(before_composer_gap);
    let composer_y = activity_y.saturating_add(activity_height);
    let hint_y = composer_y
        .saturating_add(composer_height)
        .saturating_add(after_composer_gap);
    FrameLayout {
        transcript: Rect {
            x: 0,
            y: 0,
            width,
            height: transcript_height,
        },
        activity: Rect {
            x: 0,
            y: activity_y,
            width,
            height: activity_height,
        },
        footer: Rect {
            x: 0,
            y: composer_y,
            width,
            height: height.saturating_sub(composer_y),
        },
        composer: Rect {
            x: 0,
            y: composer_y,
            width,
            height: composer_height,
        },
        menu: Rect {
            x: 0,
            y: composer_y.saturating_add(composer_height),
            width,
            height: 0,
        },
        hint: Rect {
            x: 0,
            y: hint_y,
            width,
            height: hint_height,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_planner_never_exceeds_tiny_terminal() {
        for width in 0..=2 {
            for height in 0..=3 {
                let frame = plan_flow(width, height, 4, 3, 4, 5, 1);
                for rect in [
                    frame.transcript,
                    frame.activity,
                    frame.footer,
                    frame.composer,
                    frame.menu,
                    frame.hint,
                ] {
                    assert!(rect.x.saturating_add(rect.width) <= width);
                    assert!(rect.y.saturating_add(rect.height) <= height);
                }
            }
        }
    }

    #[test]
    fn flow_places_a_short_transcript_before_the_composer() {
        let frame = plan_flow(80, 24, 1, 0, 1, 0, 1);
        assert_eq!(
            frame.transcript,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 1
            }
        );
        assert_eq!(frame.composer.y, 2);
        assert_eq!(frame.hint.y, 4);
    }
}
