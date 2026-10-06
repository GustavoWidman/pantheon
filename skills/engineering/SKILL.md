---
name: engineering
description: Implement, debug, review or optimize software with focused changes and evidence from the actual behavior.
---
Read repository instructions and trace the affected behavior before changing it.
Define the intended outcome and the data ownership or interface that expresses
it. Prefer a small coherent change. Remove obsolete paths when replacing them;
preserve unrelated work and public compatibility where the task requires it.

For a bug, reproduce the symptom and eliminate hypotheses using runtime evidence.
Verify the original reproduction after the fix. For performance, measure a
baseline, identify the limiting work and compare equivalent post-change runs.
For a feature, check the user-visible result rather than only compilation.
Use focused tests and the project's formatting, build and release conventions.
Tests should exercise behavior and fail on the actual defect.

Delegate substantial independent work with a clear goal, exclusive write scope,
relevant context, acceptance criteria and expected evidence. Use separate worktrees
for competing implementations. Keep tightly coupled changes under one owner.
Review consequential worker findings against the artifact. Independent review
helps with difficult decisions, but model agreement is not proof of correctness.
Scale planning and verification to the task. Do simple edits directly.

Report what changed, why, how it was verified and material limitations. If two
attempts fail for the same reason, examine the premise before adding another
workaround. Turn recurring mechanical mistakes into types, checks or tests when
possible. Keep judgment guidance concise and specific.
