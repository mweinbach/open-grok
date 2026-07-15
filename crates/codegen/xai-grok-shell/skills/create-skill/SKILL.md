---
name: create-skill
description: >
  Interactively create a new Grok skill (SKILL.md + optional scripts/references).
  Use when the user wants to create a skill, scaffold a skill, or runs /create-skill.
metadata:
  short-description: "Create a new Grok skill"
---

# Create Skill

Interactively gather requirements from the user and create a fully working Grok skill on disk.

## Step 1: Gather information

Ask the user the following questions **one at a time as regular conversation questions** (do NOT use structured option prompts for free-text inputs):

1. **Skill name** - ask the user to type a name. Lowercase letters (a-z), digits (0-9), and hyphens (-) only. Must start and end with a letter or digit. Must be 2-64 characters long (e.g. `deploy-k8s`). Validate the name before proceeding.
2. **Scope** - present the user with two options:
   - **Project** (Recommended): `<repo-root>/.grok/skills/<name>/SKILL.md` - available only in this repo, shareable with teammates
   - **User**: `~/.grok/skills/<name>/SKILL.md` - available in all projects
   - Default to **Project** if inside a git repo, otherwise **User**.
3. **What it should do** - ask the user to describe the workflow, paste an example prompt they keep repeating, or explain the task the skill should automate.

## Step 2: Draft the description

Write a `description` frontmatter value that includes:
- What the skill does (1-2 sentences)
- Trigger phrases and keywords so Grok knows when to auto-invoke it
- The slash command name (e.g. "Use when the user runs /deploy-k8s")

Show the drafted description to the user and let them approve or edit it.

## Step 3: Create the directory

Run this bash command to create the skill directory:

```bash
mkdir -p <SKILL_DIR>
```

Where `<SKILL_DIR>` is:
- User scope: `~/.grok/skills/<name>`
- Project scope: `<repo-root>/.grok/skills/<name>`

If the skill needs helper scripts, also create `<SKILL_DIR>/scripts/`.
If the skill needs reference docs, also create `<SKILL_DIR>/references/`.

## Step 4: Write SKILL.md

Use `search_replace` with an empty `old_string` to create the file at `<SKILL_DIR>/SKILL.md`.

The file MUST follow this exact format:

```
---
name: <skill-name>
description: <the description from Step 2>
---

<markdown body with instructions, steps, code blocks>
```

Also write any supporting files (scripts, references) using the same create method.

## Step 5: Verify and confirm

1. Run `cat <SKILL_DIR>/SKILL.md` to verify the file was written correctly.
2. Tell the user the skill is ready and how to use it:
   - Slash command: `/<skill-name>`
   - TUI menu: `/skills <skill-name>`
   - Automatic: Grok will invoke it when the description matches user intent
3. Tell the user the skill should appear in the slash menu within a few seconds (skills auto-reload when files change on disk).

## Guidelines

- Keep the SKILL.md body focused and actionable. It is a prompt for the agent, not documentation.
- The `description` field is critical. It controls auto-invocation. Be specific with trigger words.
- Prefer referencing existing CLI tools over writing custom scripts.
- Do NOT skip creating the directory. The file will fail to save without it.
- Always use absolute paths when creating files to avoid writing to the wrong location.