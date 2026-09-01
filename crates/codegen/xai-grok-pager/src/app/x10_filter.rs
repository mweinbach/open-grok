use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent};

use super::event_loop::TimedInputEvent;

pub(super) struct X10ReassemblyFilter {
    held: Option<HeldReport>,
}

struct HeldReport {
    mouse: MouseEvent,
    arrived_at: Instant,
}

const MAX_COMPLETION_GAP: Duration = Duration::from_millis(50);

impl X10ReassemblyFilter {
    pub(super) fn new() -> Self {
        Self { held: None }
    }

    pub(super) fn filter(&mut self, events: Vec<TimedInputEvent>) -> Vec<TimedInputEvent> {
        let mut result = Vec::with_capacity(events.len());
        let mut reassembled_count = 0usize;

        for ev in events {
            if let Some(held) = self.held.take() {
                let within_gap =
                    ev.arrived_at.saturating_duration_since(held.arrived_at) <= MAX_COMPLETION_GAP;
                match displaced_row_byte(&ev.event) {
                    Some(row_byte) if within_gap => {
                        result.push(reconstruct(&held, row_byte));
                        reassembled_count += 1;
                        continue;
                    }
                    _ => result.push(TimedInputEvent {
                        event: Event::Mouse(held.mouse),
                        arrived_at: held.arrived_at,
                    }),
                }
            }

            match ev.event {
                Event::Mouse(mouse_event) if is_mangled_shape(&mouse_event) => {
                    self.held = Some(HeldReport {
                        mouse: mouse_event,
                        arrived_at: ev.arrived_at,
                    });
                }
                _ => result.push(ev),
            }
        }

        if reassembled_count > 0 {
            tracing::debug!(reassembled_count, "reassembled mangled X10 mouse reports");
        }

        result
    }
}

fn is_mangled_shape(mouse_event: &MouseEvent) -> bool {
    matches!(mouse_event.column, 161 | 162) && (95..=158).contains(&mouse_event.row)
}

fn displaced_row_byte(event: &Event) -> Option<u16> {
    let Event::Key(key_event) = event else {
        return None;
    };
    if key_event.kind != KeyEventKind::Press
        || !(key_event.modifiers == KeyModifiers::NONE
            || key_event.modifiers == KeyModifiers::SHIFT)
    {
        return None;
    }
    match key_event.code {
        KeyCode::Char(character) if (0x21..=0xFF).contains(&(character as u32)) => {
            Some(character as u16)
        }
        KeyCode::Backspace if key_event.modifiers == KeyModifiers::NONE => Some(0x7F),
        _ => None,
    }
}

fn reconstruct(held: &HeldReport, row_byte: u16) -> TimedInputEvent {
    let lead = held.mouse.column + 33;
    let continuation = held.mouse.row + 33;
    let column_byte = ((lead & 0x03) << 6) | (continuation & 0x3F);
    TimedInputEvent {
        event: Event::Mouse(MouseEvent {
            kind: held.mouse.kind,
            column: column_byte - 33,
            row: row_byte - 33,
            modifiers: held.mouse.modifiers,
        }),
        arrived_at: held.arrived_at,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use crossterm::event::{KeyEvent, KeyEventState, MouseButton, MouseEventKind};

    use super::*;

    fn test_instant() -> Instant {
        static NOW: OnceLock<Instant> = OnceLock::new();
        *NOW.get_or_init(Instant::now)
    }

    fn timed(event: Event) -> TimedInputEvent {
        TimedInputEvent {
            event,
            arrived_at: test_instant(),
        }
    }

    fn press_mods(code: KeyCode, modifiers: KeyModifiers) -> TimedInputEvent {
        timed(Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> TimedInputEvent {
        timed(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }))
    }

    fn mangled_c2_col100() -> TimedInputEvent {
        mouse(MouseEventKind::Moved, 161, 99)
    }

    #[test]
    fn reconstructs_c2_report_in_one_batch() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].event,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 99,
                row: 47,
                modifiers: KeyModifiers::empty(),
            })
        );
    }

    #[test]
    fn reconstructs_c3_report() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mouse(MouseEventKind::Moved, 162, 99),
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
        ]);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0].event, Event::Mouse(mouse_event) if mouse_event.column == 163 && mouse_event.row == 47)
        );
    }

    #[test]
    fn reconstructs_non_moved_kind() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mouse(MouseEventKind::Drag(MouseButton::Left), 161, 99),
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
        ]);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0].event,
            Event::Mouse(mouse_event) if mouse_event.kind == MouseEventKind::Drag(MouseButton::Left)
                && mouse_event.column == 99
                && mouse_event.row == 47
        ));
    }

    #[test]
    fn reconstructs_shape_that_is_in_bounds_on_large_terminals() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mouse(MouseEventKind::Moved, 162, 115),
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
        ]);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0].event, Event::Mouse(mouse_event) if mouse_event.column == 179 && mouse_event.row == 47)
        );
    }

    #[test]
    fn reconstructs_report_split_across_batches() {
        let mut filter = X10ReassemblyFilter::new();
        assert!(filter.filter(vec![mangled_c2_col100()]).is_empty());
        let out = filter.filter(vec![press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT)]);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0].event, Event::Mouse(mouse_event) if mouse_event.column == 99 && mouse_event.row == 47)
        );
    }

    #[test]
    fn reconstructs_latin1_row_byte() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            press_mods(KeyCode::Char('\u{A0}'), KeyModifiers::NONE),
        ]);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0].event, Event::Mouse(mouse_event) if mouse_event.column == 99 && mouse_event.row == 127)
        );
    }

    #[test]
    fn reconstructs_backspace_row_byte() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            press_mods(KeyCode::Backspace, KeyModifiers::NONE),
        ]);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0].event, Event::Mouse(mouse_event) if mouse_event.column == 99 && mouse_event.row == 94)
        );
    }

    #[test]
    fn stale_completion_is_not_consumed() {
        let mut filter = X10ReassemblyFilter::new();
        assert!(filter.filter(vec![mangled_c2_col100()]).is_empty());
        let late_key = TimedInputEvent {
            arrived_at: test_instant() + Duration::from_millis(200),
            ..press_mods(KeyCode::Char('q'), KeyModifiers::NONE)
        };
        let out = filter.filter(vec![late_key]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event, mangled_c2_col100().event);
        assert!(matches!(
            out[1].event,
            Event::Key(key) if key.code == KeyCode::Char('q')
        ));
    }

    #[test]
    fn candidate_followed_by_other_event_is_released() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            timed(Event::FocusGained),
            press_mods(KeyCode::Char('a'), KeyModifiers::NONE),
        ]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].event, mangled_c2_col100().event);
        assert_eq!(out[1].event, Event::FocusGained);
        assert_eq!(
            out[2].event,
            press_mods(KeyCode::Char('a'), KeyModifiers::NONE).event
        );
    }

    #[test]
    fn coordinates_outside_the_magic_shape_are_untouched() {
        for candidate in [
            mouse(MouseEventKind::Moved, 160, 99),
            mouse(MouseEventKind::Moved, 163, 99),
            mouse(MouseEventKind::Moved, 161, 94),
            mouse(MouseEventKind::Moved, 161, 159),
        ] {
            let out = X10ReassemblyFilter::new().filter(vec![
                candidate,
                press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
            ]);
            assert_eq!(out.len(), 2);
        }
    }

    #[test]
    fn plain_typing_is_untouched() {
        let out = X10ReassemblyFilter::new().filter(vec![
            press_mods(KeyCode::Char('P'), KeyModifiers::SHIFT),
            press_mods(KeyCode::Char('a'), KeyModifiers::NONE),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn ordinary_mouse_events_are_untouched() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mouse(MouseEventKind::Moved, 80, 20),
            mouse(MouseEventKind::ScrollUp, 119, 49),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn non_coordinate_key_does_not_complete() {
        let out = X10ReassemblyFilter::new().filter(vec![
            mangled_c2_col100(),
            press_mods(KeyCode::Char(' '), KeyModifiers::NONE),
        ]);
        assert_eq!(out.len(), 2);
    }
}
