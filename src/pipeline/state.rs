use crate::db::RunStatus;

impl RunStatus {
    /// Determine the next state in the pipeline based on review findings and cycle count.
    ///
    /// - After implementing, always review.
    /// - If reviewer finds issues and we haven't hit max cycles, fix.
    /// - If reviewer finds issues at max cycles, move to awaiting merge (unresolved
    ///   findings are posted on the PR for human review, not treated as failure).
    /// - Clean review goes to awaiting merge (waiting for PR to be merged).
    /// - After fixing, go back to reviewing.
    /// - After awaiting merge, proceed to merging.
    /// - After merging, complete.
    #[must_use]
    pub const fn next(self, has_findings: bool, cycle: u32) -> Self {
        match self {
            Self::Pending => Self::Implementing,
            Self::Implementing | Self::Fixing => Self::Reviewing,
            Self::Reviewing if has_findings && cycle < 2 => Self::Fixing,
            Self::Reviewing => Self::AwaitingMerge,
            Self::AwaitingMerge => Self::Merging,
            Self::Merging => Self::Complete,
            Self::Complete | Self::Failed => Self::Failed,
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const ALL_STATUSES: [RunStatus; 8] = [
        RunStatus::Pending,
        RunStatus::Implementing,
        RunStatus::Reviewing,
        RunStatus::Fixing,
        RunStatus::AwaitingMerge,
        RunStatus::Merging,
        RunStatus::Complete,
        RunStatus::Failed,
    ];

    proptest! {
        #[test]
        fn next_never_panics(idx in 0..8usize, has_findings: bool, cycle in 0..50u32) {
            let status = ALL_STATUSES[idx];
            // Should never panic regardless of inputs
            let _ = status.next(has_findings, cycle);
        }

        #[test]
        fn terminal_states_stay_terminal(has_findings: bool, cycle in 0..50u32) {
            assert!(RunStatus::Complete.next(has_findings, cycle).is_terminal());
            assert!(RunStatus::Failed.next(has_findings, cycle).is_terminal());
        }

        #[test]
        fn reviewing_with_findings_past_max_awaits_merge(cycle in 2..50u32) {
            assert_eq!(RunStatus::Reviewing.next(true, cycle), RunStatus::AwaitingMerge);
        }

        #[test]
        fn reviewing_clean_always_awaits_merge(cycle in 0..50u32) {
            assert_eq!(RunStatus::Reviewing.next(false, cycle), RunStatus::AwaitingMerge);
        }
    }

    #[test]
    fn pending_to_implementing() {
        assert_eq!(RunStatus::Pending.next(false, 0), RunStatus::Implementing);
    }

    #[test]
    fn implementing_to_reviewing() {
        assert_eq!(RunStatus::Implementing.next(false, 0), RunStatus::Reviewing);
    }

    #[test]
    fn clean_review_to_awaiting_merge() {
        assert_eq!(RunStatus::Reviewing.next(false, 1), RunStatus::AwaitingMerge);
    }

    #[test]
    fn awaiting_merge_to_merging() {
        assert_eq!(RunStatus::AwaitingMerge.next(false, 0), RunStatus::Merging);
    }

    #[test]
    fn findings_under_max_cycles_to_fixing() {
        assert_eq!(RunStatus::Reviewing.next(true, 1), RunStatus::Fixing);
    }

    #[test]
    fn findings_at_max_cycles_to_awaiting_merge() {
        assert_eq!(RunStatus::Reviewing.next(true, 2), RunStatus::AwaitingMerge);
    }

    #[test]
    fn fixing_back_to_reviewing() {
        assert_eq!(RunStatus::Fixing.next(false, 1), RunStatus::Reviewing);
    }

    #[test]
    fn merging_to_complete() {
        assert_eq!(RunStatus::Merging.next(false, 0), RunStatus::Complete);
    }

    #[test]
    fn terminal_states_go_to_failed() {
        assert_eq!(RunStatus::Complete.next(false, 0), RunStatus::Failed);
        assert_eq!(RunStatus::Failed.next(false, 0), RunStatus::Failed);
    }

    #[test]
    fn is_terminal() {
        assert!(RunStatus::Complete.is_terminal());
        assert!(RunStatus::Failed.is_terminal());
        assert!(!RunStatus::Pending.is_terminal());
        assert!(!RunStatus::Implementing.is_terminal());
        assert!(!RunStatus::Reviewing.is_terminal());
        assert!(!RunStatus::Fixing.is_terminal());
        assert!(!RunStatus::AwaitingMerge.is_terminal());
        assert!(!RunStatus::Merging.is_terminal());
    }
}
