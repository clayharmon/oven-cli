use crate::db::RunStatus;

impl RunStatus {
    /// Determine the next state in the pipeline based on review findings and cycle count.
    ///
    /// - After implementing, always review.
    /// - If reviewer finds issues and we haven't hit max cycles, fix.
    /// - If reviewer finds issues at max cycles, fail.
    /// - Clean review goes to merging.
    /// - After fixing, go back to reviewing.
    /// - After merging, complete.
    #[must_use]
    pub const fn next(self, has_findings: bool, cycle: u32) -> Self {
        match self {
            Self::Pending => Self::Implementing,
            Self::Implementing | Self::Fixing => Self::Reviewing,
            Self::Reviewing if has_findings && cycle < 2 => Self::Fixing,
            Self::Reviewing if has_findings => Self::Failed,
            Self::Reviewing => Self::Merging,
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
    use super::*;

    #[test]
    fn pending_to_implementing() {
        assert_eq!(RunStatus::Pending.next(false, 0), RunStatus::Implementing);
    }

    #[test]
    fn implementing_to_reviewing() {
        assert_eq!(RunStatus::Implementing.next(false, 0), RunStatus::Reviewing);
    }

    #[test]
    fn clean_review_to_merging() {
        assert_eq!(RunStatus::Reviewing.next(false, 1), RunStatus::Merging);
    }

    #[test]
    fn findings_under_max_cycles_to_fixing() {
        assert_eq!(RunStatus::Reviewing.next(true, 1), RunStatus::Fixing);
    }

    #[test]
    fn findings_at_max_cycles_to_failed() {
        assert_eq!(RunStatus::Reviewing.next(true, 2), RunStatus::Failed);
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
        assert!(!RunStatus::Merging.is_terminal());
    }
}
