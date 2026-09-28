//! History refresh decisions for repository-feed publications (#852).
//! Planner tokens never enter this module: only history Frames are compared.
//! last_edited_by: codex
//! **Signed:** codex · 2026-09-28T10:50:28-04:00

use super::core::{Frame, GraphCore};
use crate::features::history::core::HistoryPhase;
use git_vista_protocol::GenerationToken;

/// The displayed history a live Frame probe is allowed to retire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryProbe {
    epoch: u64,
    generation: GenerationToken,
    repo: String,
    pub worktree: String,
}

impl HistoryProbe {
    /// A snapshot during seed load waits for Ready; callers track both inputs.
    pub fn for_ready(
        graph: &GraphCore,
        phase: HistoryPhase,
        seed_epoch: u64,
        frame: &Frame,
    ) -> Option<Self> {
        let epoch = graph.epoch();
        if graph.view().is_historical()
            || phase != (HistoryPhase::Ready { epoch })
            || seed_epoch != epoch
        {
            return None;
        }
        Some(Self {
            epoch,
            generation: frame.generation.clone(),
            repo: frame.repo_id.clone()?,
            worktree: frame.worktree_id.clone()?,
        })
    }

    /// Re-check the displayed seed at delivery, then compare like recipes.
    /// A settlement/selection/Refresh that retired our epoch wins the race.
    /// `None` means the probe failed, never evidence that history is current.
    pub fn apply(
        &self,
        graph: &mut GraphCore,
        phase: HistoryPhase,
        seed_epoch: u64,
        displayed: &Frame,
        live: Option<&Frame>,
    ) -> bool {
        if Self::for_ready(graph, phase, seed_epoch, displayed).as_ref() != Some(self) {
            return false;
        }
        let Some(live) = live else { return false };
        if live.repo_id.as_deref() != Some(self.repo.as_str())
            || live.worktree_id.as_deref() != Some(self.worktree.as_str())
            || live.generation == self.generation
        {
            return false;
        }
        graph.force_bump();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::core_traits::{Invalidate, InvalidateScope};

    fn frame(generation: &str) -> Frame {
        serde_json::from_value(serde_json::json!({
            "generation": generation, "refs": [], "head_branch": "main",
            "branch_colors": [], "repo_label": "fixture", "repo_id": "repo-1",
            "worktree_id": "worktree-1", "read_only": false, "resettable": false,
            "repo_url": null, "remote_web_url": null
        }))
        .unwrap()
    }

    fn ready(graph: &GraphCore) -> HistoryPhase {
        HistoryPhase::Ready {
            epoch: graph.epoch(),
        }
    }

    #[test]
    fn external_head_ref_or_history_change_remounts_once() {
        for new in ["history-new-head", "history-new-ref", "history-new-commit"] {
            let mut graph = GraphCore::default();
            let old = frame("history-before");
            let probe = HistoryProbe::for_ready(&graph, ready(&graph), 0, &old).unwrap();
            assert!(probe.apply(
                &mut graph,
                HistoryPhase::Ready { epoch: 0 },
                0,
                &old,
                Some(&frame(new))
            ));
            assert_eq!(graph.epoch(), 1);
            assert!(!probe.apply(
                &mut graph,
                HistoryPhase::Ready { epoch: 1 },
                0,
                &old,
                Some(&frame(new))
            ));
            assert_eq!(
                graph.epoch(),
                1,
                "a repeated reply cannot retire another epoch"
            );
        }
    }

    #[test]
    fn worktree_only_publication_keeps_canvas_even_with_unrelated_planner_generation() {
        let mut graph = GraphCore::at_generation("planner-dirty");
        let displayed = frame("history-same");
        let probe = HistoryProbe::for_ready(&graph, ready(&graph), 0, &displayed).unwrap();
        assert!(!probe.apply(
            &mut graph,
            HistoryPhase::Ready { epoch: 0 },
            0,
            &displayed,
            Some(&frame("history-same"))
        ));
        assert_eq!(graph.epoch(), 0);
    }

    #[test]
    fn settled_in_app_write_fences_old_probe_and_new_seed_needs_no_second_remount() {
        let mut graph = GraphCore::default();
        let old = frame("history-before");
        let probe = HistoryProbe::for_ready(&graph, ready(&graph), 0, &old).unwrap();
        graph.on_invalidate(&Invalidate {
            generation: Some(GenerationToken::new("planner-after").unwrap()),
            scope: InvalidateScope::Everything,
        });
        let live = frame("history-after");
        assert!(!probe.apply(
            &mut graph,
            HistoryPhase::Ready { epoch: 1 },
            0,
            &old,
            Some(&live)
        ));
        let probe = HistoryProbe::for_ready(&graph, ready(&graph), 1, &live).unwrap();
        assert!(!probe.apply(
            &mut graph,
            HistoryPhase::Ready { epoch: 1 },
            1,
            &live,
            Some(&live)
        ));
        assert_eq!(graph.epoch(), 1);
    }

    #[test]
    fn historical_view_never_requests_or_accepts_live_history() {
        let mut graph = GraphCore::default();
        let old = frame("history-before");
        let probe = HistoryProbe::for_ready(&graph, ready(&graph), 0, &old).unwrap();
        graph.show_as_of("saved-view".into(), 100, Some("worktree-1".into()));
        assert!(HistoryProbe::for_ready(&graph, ready(&graph), 1, &old).is_none());
        assert!(!probe.apply(
            &mut graph,
            HistoryPhase::Ready { epoch: 1 },
            0,
            &old,
            Some(&frame("history-live"))
        ));
        assert!(graph.view().is_historical());
        assert_eq!(graph.epoch(), 1);
    }

    #[test]
    fn snapshot_during_seed_load_is_rechecked_on_ready() {
        let mut graph = GraphCore::default();
        let old = frame("history-before");
        assert!(
            HistoryProbe::for_ready(&graph, HistoryPhase::SeedLoading { epoch: 0 }, 0, &old)
                .is_none()
        );
        let probe = HistoryProbe::for_ready(&graph, ready(&graph), 0, &old).unwrap();
        assert!(probe.apply(
            &mut graph,
            HistoryPhase::Ready { epoch: 0 },
            0,
            &old,
            Some(&frame("history-after"))
        ));
        assert_eq!(graph.epoch(), 1);
    }

    #[test]
    fn missing_identity_wrong_repository_and_failed_reads_cannot_retire_history() {
        let mut graph = GraphCore::default();
        let old = frame("history-before");
        let probe = HistoryProbe::for_ready(&graph, ready(&graph), 0, &old).unwrap();
        let mut wrong = frame("history-after");
        wrong.worktree_id = Some("another-worktree".into());
        assert!(!probe.apply(
            &mut graph,
            HistoryPhase::Ready { epoch: 0 },
            0,
            &old,
            Some(&wrong)
        ));
        wrong = frame("history-after");
        wrong.repo_id = Some("another-repo".into());
        assert!(!probe.apply(
            &mut graph,
            HistoryPhase::Ready { epoch: 0 },
            0,
            &old,
            Some(&wrong)
        ));
        assert!(!probe.apply(&mut graph, HistoryPhase::Ready { epoch: 0 }, 0, &old, None));
        wrong.repo_id = None;
        assert!(HistoryProbe::for_ready(&graph, ready(&graph), 0, &wrong).is_none());
        assert_eq!(graph.epoch(), 0);
    }

    #[test]
    fn retired_or_failed_seed_does_not_start_a_probe() {
        let graph = GraphCore::default();
        for phase in [
            HistoryPhase::SeedError { epoch: 0 },
            HistoryPhase::DriftReloading { epoch: 0 },
            HistoryPhase::Ready { epoch: 1 },
        ] {
            assert!(HistoryProbe::for_ready(&graph, phase, 0, &frame("history-old")).is_none());
        }
        assert!(HistoryProbe::for_ready(&graph, ready(&graph), 1, &frame("history-old")).is_none());
    }
}
