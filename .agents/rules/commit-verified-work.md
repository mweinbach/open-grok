---
description: Always commit verified work in logical units as it lands
trigger: always_on
---

# Commit Verified Work Progressively

Always commit tested and verified units as work is completed:
1. **Never wait until the end** of a large task or wave to create a giant commit.
2. **Each commit must be a coherent, verified slice** with a descriptive imperative subject (≤72 chars) and an explanation of what changed and why it mattered.
3. **Never commit broken or unverified trees**: always run package-scoped compilation and relevant test suites before committing.
4. **Stage only owned files**: never sweep concurrent or unrelated edits into a commit.
