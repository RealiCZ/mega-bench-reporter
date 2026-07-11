//! The two first-class metric lanes the reporter tracks per commit: criterion
//! walltime and callgrind instruction counts. Both run the same protocol
//! (ratios vs a baseline subject → rolling-median regression check → events);
//! [`Lane`] is the single value that tells the shared machinery which lane it
//! is working, so the walltime and instructions paths stay symmetric instead
//! of duplicated.

/// One of the reporter's two metric lanes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lane {
    /// Criterion per-call walltime (the original lane).
    Walltime,
    /// CodSpeed/callgrind instruction counts (`Ir`).
    Instructions,
}

impl Lane {
    /// The `metric` value events from this lane carry in their JSON: `None`
    /// for the walltime lane — skipped in the serialized event so pre-lane
    /// consumers see byte-identical events — and `Some("instructions")` for
    /// the instruction-count lane. Convert to the serialized `Option<String>`
    /// at the event-emission boundary.
    pub fn metric_field(self) -> Option<&'static str> {
        match self {
            Lane::Walltime => None,
            Lane::Instructions => Some("instructions"),
        }
    }

    /// Human-readable lane name for log lines and stderr notes.
    pub fn display_name(self) -> &'static str {
        match self {
            Lane::Walltime => "walltime",
            Lane::Instructions => "instructions",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metric_field_is_the_serialized_event_marker() {
        // The exact byte contract the event JSON depends on: absent for
        // walltime, "instructions" for the instructions lane.
        assert_eq!(Lane::Walltime.metric_field(), None);
        assert_eq!(Lane::Instructions.metric_field(), Some("instructions"));
    }

    #[test]
    fn test_display_name() {
        assert_eq!(Lane::Walltime.display_name(), "walltime");
        assert_eq!(Lane::Instructions.display_name(), "instructions");
    }
}
