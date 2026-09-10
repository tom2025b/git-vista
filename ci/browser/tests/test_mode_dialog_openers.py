"""Source-structure guard for the shared FULL-mode openers.

The browser specs prove that the locators describe the rendered app and that
FULL-only operations work. This guard has the narrower job of keeping the
proven wait -> click -> dismissal sequence from regressing to an instant
visibility sample again.
"""

from pathlib import Path
import unittest


HELPERS = (Path(__file__).parent / "helpers.mjs").read_text(encoding="utf-8")


def opener_source(name: str) -> str:
    marker = f"export async function {name}(page) {{"
    start = HELPERS.find(marker)
    if start == -1:
        raise AssertionError(f"{name} must still exist")
    next_opener = HELPERS.find("\nexport async function ", start + 1)
    return HELPERS[start : next_opener if next_opener != -1 else None]


class FullModeOpeners(unittest.TestCase):
    def assert_hardened(self, name: str) -> None:
        source = opener_source(name)
        self.assertNotIn(
            "full.isVisible().catch",
            source,
            "a guaranteed mode dialog must never be reduced to an instant sample",
        )

        wait = source.find(
            "await expect(full, 'the mode dialog follows opening a repository').toBeVisible()"
        )
        click = source.find("await full.click()")
        picker_gone = source.find(
            "await expect(entry, 'the picker must be dismissed before graph interactions').toHaveCount(0)"
        )
        mode_gone = source.find(
            "await expect(full, 'the mode dialog must be dismissed before graph interactions').toHaveCount(0)"
        )

        self.assertGreaterEqual(wait, 0, "the guaranteed dialog must be awaited")
        self.assertGreater(click, wait, "FULL mode must be clicked only after it is visible")
        self.assertGreater(
            picker_gone, click, "the picker dismissal must be checked after the click"
        )
        self.assertGreater(
            mode_gone, picker_gone, "the mode-dialog dismissal must also be checked"
        )

    def test_stash_repo_opener(self) -> None:
        self.assert_hardened("openStashRepo")

    def test_worktree_repo_opener(self) -> None:
        self.assert_hardened("openWorktreeRepo")


if __name__ == "__main__":
    unittest.main()
