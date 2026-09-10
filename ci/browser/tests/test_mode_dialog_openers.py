"""Source-structure guard for the repository mode-dialog openers.

Playwright proves that the locators describe the rendered app. This cheap CI
check has the narrower job of keeping the seven #830 sites on the proven
wait -> click -> dismiss overlays -> accept repository Frame sequence.
"""

from pathlib import Path
import unittest


TESTS = Path(__file__).parent


def function_source(path: str, name: str, exported: bool = False) -> str:
    source = (TESTS / path).read_text(encoding="utf-8")
    prefix = "export " if exported else ""
    marker = f"{prefix}async function {name}(page"
    start = source.find(marker)
    if start == -1:
        raise AssertionError(f"{name} must still exist in {path}")
    end = source.find("\n}\n", start)
    if end == -1:
        raise AssertionError(f"{name} must still have a function boundary in {path}")
    return source[start : end + 2]


class RepositoryModeOpeners(unittest.TestCase):
    def assert_hardened(
        self,
        path: str,
        name: str,
        dialog: str,
        repo_text: str,
        *,
        exported: bool = False,
    ) -> None:
        source = function_source(path, name, exported)
        self.assertNotIn(
            ".isVisible().catch",
            source,
            "a guaranteed mode dialog must never be reduced to an instant sample",
        )

        wait = source.find(
            f"await expect({dialog}, 'the mode dialog follows opening a repository').toBeVisible({{"
        )
        timeout = source.find("timeout: 20_000", wait)
        click = source.find(f"await {dialog}.click()")
        picker_gone = source.find("the picker must be dismissed before graph interactions")
        mode_gone = source.find("the mode dialog must be dismissed before graph interactions")
        graph = source.find("Commit history graph")
        accepted_repo = source.find(repo_text, graph)
        node = source.find("circle.node-hit")

        self.assertGreaterEqual(wait, 0, "the guaranteed dialog must be awaited")
        self.assertGreater(timeout, wait, "the dialog wait must carry the 20-second budget")
        self.assertGreater(click, timeout, "the mode must be clicked only after it is visible")
        self.assertGreater(picker_gone, click, "the picker dismissal must follow the click")
        self.assertGreater(mode_gone, picker_gone, "the mode-dialog dismissal must also be checked")
        self.assertGreater(graph, mode_gone, "graph readiness must follow overlay dismissal")
        self.assertGreater(accepted_repo, graph, "readiness must identify the selected repository")
        self.assertGreater(node, accepted_repo, "nodes must belong to the accepted repository Frame")

    def test_stash_repo_opener(self) -> None:
        self.assert_hardened("helpers.mjs", "openStashRepo", "full", "STASH_REPO", exported=True)

    def test_worktree_repo_opener(self) -> None:
        self.assert_hardened(
            "helpers.mjs", "openWorktreeRepo", "full", "WORKTREE_REPO", exported=True
        )

    def test_broken_head_opener(self) -> None:
        self.assert_hardened("broken-head.spec.mjs", "openRepo", "full", "namePattern")

    def test_conflict_editor_opener(self) -> None:
        self.assert_hardened(
            "conflict-editor.spec.mjs", "openConflictRepo", "full", "'editor-repo'"
        )

    def test_conflict_panes_opener(self) -> None:
        self.assert_hardened(
            "conflict-panes.spec.mjs", "openConflictRepo", "full", "'conflict-repo'"
        )

    def test_nontext_conflicts_opener(self) -> None:
        self.assert_hardened(
            "nontext-conflicts.spec.mjs", "openNonTextRepo", "full", "'nontext-repo'"
        )

    def test_wip_collapse_opener(self) -> None:
        self.assert_hardened(
            "wip-collapse.spec.mjs", "openTwinRepo", "visualize", "/interleaved-repo/i"
        )


if __name__ == "__main__":
    unittest.main()
