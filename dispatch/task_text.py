"""Task text after the prompt → description move (doc/org-model.md §13.2).

A task carries one brief, its `description`. The `prompt` column stays (it is
never dropped), but no client should need it: old clients that still send a
prompt have it folded into the description on CREATE, a PATCH prompt is
ignored when the stored description already says it, and responses return
`prompt: ""` so an old composer cannot deliver it twice.
"""

from __future__ import annotations

INSTRUCTIONS_HEADING = "## Instructions"
# Kinds whose prompt is never folded into the description (sql/027 skips them
# too): a continuous task's contract is its record's standing_instructions,
# and a backtest's prompt is a synthesized label nothing delivers.
UNFOLDED_KINDS = frozenset({"continuous", "backtest"})


def _clean(value: str | None) -> str:
    return (value or "").strip()


def prompt_is_redundant(prompt: str | None, description: str | None,
                        name: str | None) -> bool:
    """True when `prompt` adds nothing: empty, equal to the name, or already
    contained in the description."""
    prompt = _clean(prompt)
    return not prompt or prompt == _clean(name) or prompt in _clean(description)


def fold_prompt(description: str | None, prompt: str | None,
                name: str | None) -> str | None:
    """The description to store for a CREATE that also sent a prompt.

    A redundant prompt leaves the description as given; otherwise the prompt
    is appended under `## Instructions` so it is still delivered."""
    if prompt_is_redundant(prompt, description, name):
        return description
    body = (description or "").rstrip()
    block = f"{INSTRUCTIONS_HEADING}\n\n{_clean(prompt)}"
    return f"{body}\n\n{block}" if body.strip() else block


def compose_brief(description: str | None, prompt: str | None,
                  name: str | None = None) -> str:
    """What a worker receives at launch: the description, plus a legacy
    prompt only when the description does not already contain it (rows read
    straight from the database before the backfill, or across version skew)."""
    description = _clean(description)
    if prompt_is_redundant(prompt, description, name):
        return description or _clean(prompt)
    prompt = _clean(prompt)
    return f"{description}\n\n---\n\n{prompt}" if description else prompt
