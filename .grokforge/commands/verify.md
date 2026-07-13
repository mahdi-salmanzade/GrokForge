Verify the current GrokForge change end to end.

Read `AGENTS.md`, inspect the current diff, and identify the smallest relevant test surface. Run
formatting checks, focused crate tests, and clippy with warnings denied. If the change crosses a
protocol/core/frontend boundary, trace and test the complete path. Fix regressions you introduced,
but preserve unrelated user changes. Finish with the exact checks run and any remaining risk.
