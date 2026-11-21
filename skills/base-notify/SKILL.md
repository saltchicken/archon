+++
abstract = true
description = "Abstract base: fires a desktop notification after the turn completes, self-labeled via {{skill.name}}."
name = "base-notify"
tools = ["notify"]

[[hooks]]
tool = "notify"
trigger = "after_turn"

[hooks.args]
body = "{{last_response}}"
summary = "{{skill.name}} finished"
urgency = "normal"
+++
Reusable base skill — extend it; do not invoke directly.

Any skill that sets `extends = "base-notify"` inherits an `after_turn` hook that sends a desktop notification the moment the turn completes:
- `summary` is self-labeled via `{{skill.name}}`, so each child's ping reads `<skill> finished`.
- `body` carries the assistant's final reply text automatically.
- `urgency` is `normal`.

Children do **not** need to call `notify` themselves; the inherited hook handles it. Note there is no way to opt a child *out* of an inherited hook (and a child can only *add* hooks, not replace them), so attach this base only to skills that should notify.
